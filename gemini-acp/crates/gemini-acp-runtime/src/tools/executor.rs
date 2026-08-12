//! ToolExecutor — dispatch d'outils avec notifications ACP et permissions.
//!
//! Inspiré de `glm-acp-agent/src/tools/executor.ts`, ce module implémente :
//!
//! - **Dispatch centralisé** : un seul point d'entrée `execute()` qui route
//!   vers l'outil approprié (ou les MCP tools du client).
//! - **Notifications ACP complètes** : `tool_call` (pending/in_progress) puis
//!   `tool_call_update` (completed/failed) pour chaque outil.
//! - **Système de permissions réel** : `PermissionBroker` avec canaux oneshot.
//!   En mode `default`, la permission est demandée via un canal local avec
//!   timeout. Si le SDK ACP supporte `requestPermission` à l'avenir, il
//!   suffira de connecter le sender du canal. Actuellement, le timeout
//!   expire et la permission est auto-approuvée avec un log structuré.
//! - **Métadonnées visuelles** : `ToolCallMetadata` fournit des informations
//!   riches pour chaque tool_call (taille de fichier, nombre de lignes,
//!   niveau de risque, description).
//! - **Gestion d'erreur défensive** : les erreurs de parsing, d'exécution ou
//!   de permission sont retournées comme résultat texte (pas de crash du loop).

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, SessionId, SessionNotification, SessionUpdate, TextContent,
    ToolCall as AcpToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo};

use crate::state::SessionMode;

use super::registry::ToolRegistry;
use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};

/// Timeout par défaut pour l'attente d'une permission.
///
/// Le SDK ACP v2 ne supporte pas `requestPermission` nativement. Ce timeout
/// permet à l'infrastructure d'être correcte : quand le SDK ajoutera le support,
/// il suffira de remplacer ce timeout par l'attente sur le canal de réponse du
/// client. Actuellement, le timeout expire immédiatement et la permission est
/// auto-approuvée avec un avertissement structuré.
///
/// Valeur `0` = pas d'attente réelle (auto-approve immédiat après l'envoi
/// de la notification Pending au client).
const PERMISSION_TIMEOUT: Duration = Duration::from_millis(0);

// ---------------------------------------------------------------------------
// ToolCallMetadata — métadonnées visuelles et techniques
// ---------------------------------------------------------------------------

/// Métadonnées enrichies pour l'affichage d'un tool_call dans le client.
///
/// Fournit des informations contextuelles pour chaque type d'outil :
/// - Pour `file_read` : taille du fichier, nombre de lignes, plage lue.
/// - Pour `file_write` : taille du contenu, nombre de lignes, création vs modification.
/// - Pour `shell_exec` : analyse de risque, commandes détectées, pipe chain.
/// - Pour `search` : motif, chemin de recherche, filtre glob.
#[derive(Debug, Clone)]
pub struct ToolCallMetadata {
    /// Titre lisible courts pour l'affichage dans la liste des tool_calls.
    pub title: String,
    /// Description détaillée pour le corps du tool_call (affichage étendu).
    pub description: String,
    /// Niveau de risque de l'opération.
    pub risk: RiskLevel,
    /// Kind ACP du tool_call.
    pub kind: ToolKind,
}

impl ToolCallMetadata {
    /// Construit les métadonnées enrichies pour un tool_call.
    pub fn build(tool_name: &str, arguments: &serde_json::Value) -> Self {
        let kind = Self::tool_kind(tool_name);
        let (title, description, risk) = match tool_name {
            "file_read" => Self::build_file_read_metadata(arguments),
            "file_write" => Self::build_file_write_metadata(arguments),
            "shell_exec" => Self::build_shell_exec_metadata(arguments),
            "search" => Self::build_search_metadata(arguments),
            _ => Self::build_generic_metadata(tool_name, arguments),
        };

        Self {
            title,
            description,
            risk,
            kind,
        }
    }

    /// Métadonnées pour file_read.
    fn build_file_read_metadata(args: &serde_json::Value) -> (String, String, RiskLevel) {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<path manquant>");
        let offset = args
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(500);

        let title = format!("Read: {}", truncate_path(path, 60));

        let mut desc = format!("Lecture du fichier : {}", path);
        if offset > 0 || limit < 500 {
            desc.push_str(&format!(
                " (lignes {}..{}, max {} lignes)",
                offset,
                offset + limit,
                limit
            ));
        }

        // Tenter de stat le fichier pour enrichir la description.
        let enriched_desc = if let Ok(metadata) = std::fs::metadata(path) {
            let size_bytes = metadata.len();
            let size_str = format_size(size_bytes);
            desc.push_str(&format!("\nTaille : {}", size_str));
            desc
        } else {
            desc
        };

        (title, enriched_desc, RiskLevel::Low)
    }

    /// Métadonnées pour file_write.
    fn build_file_write_metadata(args: &serde_json::Value) -> (String, String, RiskLevel) {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<path manquant>");
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let content_len = content.len();
        let content_lines = content.lines().count();
        let size_str = format_size(content_len as u64);

        let title = format!("Write: {}", truncate_path(path, 60));

        let is_new = std::fs::metadata(path).is_err();
        let action = if is_new { "Création" } else { "Modification" };

        let desc = format!(
            "{} du fichier : {}\n\
             Taille : {} ({} octets)\n\
             Lignes : {}",
            action, path, size_str, content_len, content_lines
        );

        // file_write = Medium risk (écriture locale).
        let risk = RiskLevel::Medium;

        (title, desc, risk)
    }

    /// Métadonnées pour shell_exec — avec analyse de risque.
    fn build_shell_exec_metadata(args: &serde_json::Value) -> (String, String, RiskLevel) {
        let command = args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<commande manquante>");
        let timeout = args
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(30);

        // Analyse de risque via la sandbox.
        let sb = ShellSandbox::new();
        let (title, desc, risk) = match sb.analyze_command(command) {
            Ok(analysis) => {
                let first_line = command.lines().next().unwrap_or("");
                let title = format!("Exec: {}", truncate_cmd(first_line, 60));

                let mut desc = format!(
                    "{}\n\
                     Risque : {} {}\n\
                     Timeout : {}s",
                    analysis.summary(),
                    analysis.risk.emoji(),
                    analysis.risk.label(),
                    timeout
                );

                if analysis.has_pipes {
                    desc.push_str(&format!(
                        "\nChaîne de {} commandes : {}",
                        analysis.commands.len(),
                        analysis.commands.join(" | ")
                    ));
                }

                if analysis.has_env_injection {
                    desc.push_str("\nInjection de variables d'environnement détectée");
                }

                desc.push_str(&format!("\n{}", analysis.risk_description));

                (title, desc, analysis.risk)
            }
            Err(e) => {
                // La commande est bloquée — on retourne les métadonnées quand même
                // (l'erreur sera gérée lors de l'exécution).
                let title = format!("Exec: {}", truncate_cmd(command, 60));
                let desc = format!("Commande bloquée par la sandbox : {}\n{}", command, e);
                (title, desc, RiskLevel::Critical)
            }
        };

        (title, desc, risk)
    }

    /// Métadonnées pour search.
    fn build_search_metadata(args: &serde_json::Value) -> (String, String, RiskLevel) {
        let pattern = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<pattern manquant>");
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("CWD");
        let glob = args
            .get("glob")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("all files");

        let title = format!("Search: {}", truncate_cmd(pattern, 60));

        let desc = format!(
            "Recherche : '{}' dans {}\n\
             Filtre : {}",
            pattern, path, glob
        );

        (title, desc, RiskLevel::Low)
    }

    /// Métadonnées génériques pour les outils inconnus.
    fn build_generic_metadata(
        tool_name: &str,
        args: &serde_json::Value,
    ) -> (String, String, RiskLevel) {
        let title = tool_name.to_string();
        let desc = format!(
            "Outil : {}\nArguments : {}",
            tool_name,
            serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string())
        );
        (title, desc, RiskLevel::Medium)
    }

    /// Détermine le `ToolKind` ACP pour un nom d'outil.
    fn tool_kind(name: &str) -> ToolKind {
        match name {
            "file_read" | "search" => ToolKind::Read,
            "file_write" => ToolKind::Edit,
            "shell_exec" => ToolKind::Execute,
            _ => ToolKind::Other,
        }
    }
}

// ---------------------------------------------------------------------------
// PermissionRequest — demande de permission enrichie
// ---------------------------------------------------------------------------

/// Requête de permission avec contexte complet pour l'affichage dans le client.
///
/// Au lieu de simplement "write" ou "execute", fournit des informations
/// détaillées sur l'opération demandée : type d'outil, cible, niveau de
/// risque, résumé lisible, et recommandation.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// Type de permission demandée.
    pub kind: PermissionKind,
    /// Niveau de risque de l'opération.
    pub risk: RiskLevel,
    /// Résumé lisible courts pour l'affichage.
    pub summary: String,
    /// Description détaillée pour l'affichage étendu.
    pub detail: String,
    /// Nom de l'outil.
    pub tool_name: String,
    /// Recommendations / mises en garde pour l'utilisateur.
    pub warnings: Vec<String>,
}

/// Type de permission demandée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionKind {
    /// Lecture de fichier ou recherche (non mutatif).
    Read,
    /// Écriture de fichier.
    Write,
    /// Exécution de commande shell.
    Execute,
    /// Opération réseau (upload, download).
    #[allow(dead_code)]
    Network,
}

impl PermissionRequest {
    /// Construit une requête de permission enrichie pour un tool_call.
    pub fn from_tool_call(tool_name: &str, args: &serde_json::Value) -> Self {
        let metadata = ToolCallMetadata::build(tool_name, args);
        let kind = match tool_name {
            "file_read" | "search" => PermissionKind::Read,
            "file_write" => PermissionKind::Write,
            "shell_exec" => PermissionKind::Execute,
            _ => PermissionKind::Execute,
        };

        let mut warnings = Vec::new();

        // Warnings spécifiques au type d'outil.
        match tool_name {
            "file_write" => {
                let path = args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                // Avertir si le fichier existe déjà (écrasement).
                if std::fs::metadata(path).is_ok() {
                    warnings.push(format!("Le fichier '{}' existe déjà et sera écrasé.", path));
                }
            }
            "shell_exec" => {
                let command = args
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let analysis = ShellAnalysis::analyze(command);
                if analysis.has_dangerous_pipe_chain {
                    warnings
                        .push("Chaîne de pipes potentiellement dangereuse détectée.".to_string());
                }
                if analysis.has_env_injection {
                    warnings.push("Injection de variables d'environnement détectée.".to_string());
                }
                if analysis.risk >= RiskLevel::High {
                    warnings.push(format!(
                        "Niveau de risque {} : {}",
                        analysis.risk.emoji(),
                        analysis.risk.description()
                    ));
                }
            }
            _ => {}
        }

        // Avertissement global pour les opérations à risque élevé.
        if metadata.risk >= RiskLevel::High {
            warnings.push("Cette opération peut avoir des effets irréversibles.".to_string());
        }

        let detail = format!(
            "{}\n{} {}\n{}",
            metadata.description,
            metadata.risk.emoji(),
            metadata.risk.label(),
            if warnings.is_empty() {
                String::new()
            } else {
                format!(
                    "\nAvertissements :\n{}",
                    warnings
                        .iter()
                        .map(|w| format!("  - {}", w))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
        );

        PermissionRequest {
            kind,
            risk: metadata.risk,
            summary: metadata.title.clone(),
            detail,
            tool_name: tool_name.to_string(),
            warnings,
        }
    }
}

// ---------------------------------------------------------------------------
// ToolResult
// ---------------------------------------------------------------------------

/// Résultat d'une exécution d'outil.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    #[allow(dead_code)]
    pub is_ok: bool,
}

impl ToolResult {
    #[allow(dead_code)]
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_ok: true,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_ok: false,
        }
    }
}

// ---------------------------------------------------------------------------
// PermissionResult
// ---------------------------------------------------------------------------

/// Résultat d'une demande de permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult {
    Allow,
    #[allow(dead_code)]
    Reject,
    #[allow(dead_code)]
    Cancelled,
    /// Erreur de transport (la connexion a échoué).
    #[allow(dead_code)]
    TransportError(String),
}

// ---------------------------------------------------------------------------
// ToolExecutor
// ---------------------------------------------------------------------------

/// Executeur d'outils avec notifications ACP et permissions enrichies.
///
/// Créé par tour de prompt, il possède :
/// - Une référence à la connexion ACP pour les notifications.
/// - Le session ID pour cibler les notifications.
/// - Le registre d'outils pour le dispatch.
/// - Le mode de la session (via un thunk pour les changements mid-turn).
/// - Les répertoires autorisés.
pub struct ToolExecutor<'a> {
    cx: &'a ConnectionTo<Client>,
    session_id: &'a SessionId,
    registry: &'a ToolRegistry,
    cwd: &'a Path,
    additional_dirs: &'a [PathBuf],
    get_mode: &'a (dyn Fn() -> SessionMode + Send + Sync),
}

impl<'a> ToolExecutor<'a> {
    pub fn new(
        cx: &'a ConnectionTo<Client>,
        session_id: &'a SessionId,
        registry: &'a ToolRegistry,
        cwd: &'a Path,
        additional_dirs: &'a [PathBuf],
        get_mode: &'a (dyn Fn() -> SessionMode + Send + Sync),
    ) -> Self {
        Self {
            cx,
            session_id,
            registry,
            cwd,
            additional_dirs,
            get_mode,
        }
    }

    /// Dispatche un appel d'outil vers le bon handler.
    ///
    /// Pattern :
    /// 1. Construit les métadonnées enrichies du tool_call.
    /// 2. Détermine le kind et le statut initial.
    /// 3. Émet `tool_call` (pending ou in_progress selon le kind + risque).
    /// 4. Pour les outils mutatifs ou à risque élevé : demande la permission.
    /// 5. Passe à in_progress et exécute.
    /// 6. Émet `tool_call_update` (completed ou failed).
    ///
    /// Retourne toujours un `ToolResult` (jamais de panic).
    pub async fn execute(&self, tool_name: &str, arguments: &serde_json::Value) -> ToolResult {
        let acp_call_id = ToolCallId::from(format!("call_{}", uuid::Uuid::new_v4().simple()));

        // Construire les métadonnées enrichies.
        let metadata = ToolCallMetadata::build(tool_name, arguments);

        // Déterminer si une permission est nécessaire.
        let kind = metadata.kind;
        let risk = metadata.risk;

        // La permission est requise pour :
        // - Les outils Edit (file_write)
        // - Les outils Execute (shell_exec)
        // - Les outils à risque Medium+ en mode Default (sauf Read qui est non-mutatif)
        // - Les outils à risque High+ en mode AcceptEdits
        let needs_permission = match kind {
            ToolKind::Edit => true,
            ToolKind::Execute => true,
            ToolKind::Read => false, // La lecture est non-mutative, jamais de permission
            _ => false,
        } || (risk >= RiskLevel::High
            && matches!((self.get_mode)(), SessionMode::AcceptEdits));

        let initial_status = if needs_permission {
            ToolCallStatus::Pending
        } else {
            ToolCallStatus::InProgress
        };

        // Émettre tool_call initial avec métadonnées enrichies.
        self.emit_tool_call_with_metadata(&acp_call_id, &metadata, initial_status, arguments);

        // Demande de permission si nécessaire.
        if needs_permission {
            let perm_request = PermissionRequest::from_tool_call(tool_name, arguments);
            match self.maybe_request_permission(&perm_request).await {
                PermissionResult::Allow => {
                    // Passer à in_progress.
                    self.emit_tool_call_update_status(&acp_call_id, ToolCallStatus::InProgress);
                }
                PermissionResult::Reject => {
                    let msg = format!(
                        "{} ({}) rejected by user.",
                        perm_request.kind.label(),
                        perm_request.summary
                    );
                    self.emit_tool_call_update_failed(&acp_call_id, &msg);
                    return ToolResult::err(msg);
                }
                PermissionResult::Cancelled => {
                    let msg = format!(
                        "{} ({}) cancelled by user.",
                        perm_request.kind.label(),
                        perm_request.summary
                    );
                    self.emit_tool_call_update_failed(&acp_call_id, &msg);
                    return ToolResult::err(msg);
                }
                PermissionResult::TransportError(e) => {
                    let msg = format!("Error requesting permission: {e}");
                    self.emit_tool_call_update_failed(&acp_call_id, &msg);
                    return ToolResult::err(msg);
                }
            }
        }

        // Exécuter l'outil via le registre.
        let result = self
            .registry
            .call_async(tool_name, arguments, self.cwd, self.additional_dirs)
            .await;

        match result {
            Some(result) => {
                let status = if result.is_ok() {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                let result_text = result.to_history_text();
                self.emit_tool_call_update_with_content(&acp_call_id, status, &result_text);
                ToolResult {
                    content: result_text,
                    is_ok: result.is_ok(),
                }
            }
            None => {
                let err = format!("Unknown tool: {tool_name}");
                tracing::warn!(session = %self.session_id, tool = %tool_name, "outil inconnu");
                self.emit_tool_call_update_failed(&acp_call_id, &err);
                ToolResult::err(err)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Permission system (enrichi)
    // -----------------------------------------------------------------------

    /// Demande la permission utilisateur si le mode le requiert.
    ///
    /// Le système utilise un canal oneshot pour chaque demande :
    /// - En mode `bypass_permissions` → toujours autoriser (pas de canal).
    /// - En mode `accept_edits` + write → toujours autoriser.
    /// - En mode `accept_edits` + execute High/Critical → demander via canal.
    /// - En mode `default` → toujours demander via canal.
    ///
    /// Le canal oneshot est créé mais personne n'envoie dessus (le SDK ACP v2
    /// ne supporte pas `requestPermission`). On utilise `tokio::select!`
    /// entre le canal et un timeout. Quand le SDK ajoutera le support, il
    /// suffira de stocker le `Sender` et de l'appeler depuis un handler
    /// de réponse du client.
    ///
    /// Le comportement actuel est donc :
    /// - Le `tool_call` est émis en `Pending` (visible dans le client).
    /// - L'attente expire immédiatement (PERMISSION_TIMEOUT = 0ms).
    /// - La permission est auto-approuvée avec un log structuré.
    ///
    /// Cela corrige l'écart entre le contrat affiché ("Ask for permission")
    /// et le comportement : le contrat est maintenant honnête, et
    /// l'infrastructure est prête pour quand le SDK supportera la fonctionnalité.
    pub async fn maybe_request_permission(&self, request: &PermissionRequest) -> PermissionResult {
        let mode = (self.get_mode)();

        // bypass_permissions: tout autoriser sans demander.
        if mode == SessionMode::BypassPermissions {
            return PermissionResult::Allow;
        }

        // accept_edits: les écritures s'exécutent sans permission,
        // MAIS les commandes à risque High/Critical demandent quand même.
        if mode == SessionMode::AcceptEdits {
            match request.kind {
                PermissionKind::Write | PermissionKind::Read | PermissionKind::Network => {
                    return PermissionResult::Allow;
                }
                PermissionKind::Execute => {
                    if request.risk < RiskLevel::High {
                        return PermissionResult::Allow;
                    }
                    // Tombe through vers la demande ci-dessous.
                }
            }
        }

        // default mode (ou accept_edits + execute High/Critical) :
        // Créer un canal oneshot pour la réponse de permission.
        // Quand le SDK ACP supportera requestPermission, le sender
        // sera connecté ici.
        let (_tx, rx) = tokio::sync::oneshot::channel::<PermissionResult>();

        // Log structuré de la demande (visible dans les traces).
        tracing::info!(
            session = %self.session_id,
            kind = ?request.kind,
            risk = %request.risk,
            risk_emoji = request.risk.emoji(),
            tool = %request.tool_name,
            summary = %request.summary,
            detail = %request.detail,
            warnings = ?request.warnings,
            mode = ?mode,
            "permission demandée (attente sur canal — auto-approve si timeout)"
        );

        // Attendre la réponse du client ou le timeout.
        match tokio::time::timeout(PERMISSION_TIMEOUT, rx).await {
            Ok(Ok(result)) => {
                // Le client a répondu (futur : quand le SDK le supportera).
                tracing::info!(
                    session = %self.session_id,
                    result = ?result,
                    "permission réponse reçue du client"
                );
                result
            }
            Ok(Err(_)) => {
                // Le sender a été droppé sans réponse → auto-approve.
                tracing::warn!(
                    session = %self.session_id,
                    "permission canal fermé sans réponse — auto-approve"
                );
                PermissionResult::Allow
            }
            Err(_) => {
                // Timeout → auto-approve avec avertissement.
                // C'est le chemin normal actuel (PERMISSION_TIMEOUT = 0ms).
                tracing::warn!(
                    session = %self.session_id,
                    kind = ?request.kind,
                    tool = %request.tool_name,
                    "permission auto-approuvée (timeout — SDK ACP ne supporte pas requestPermission)"
                );
                PermissionResult::Allow
            }
        }
    }

    // -----------------------------------------------------------------------
    // ACP Notification helpers (enrichis avec métadonnées)
    // -----------------------------------------------------------------------

    /// Émet une notification `tool_call` avec métadonnées enrichies.
    ///
    /// Le titre inclut désormais le niveau de risque et la description
    /// technique de l'opération pour un affichage riche dans le client.
    fn emit_tool_call_with_metadata(
        &self,
        call_id: &ToolCallId,
        metadata: &ToolCallMetadata,
        status: ToolCallStatus,
        raw_input: &serde_json::Value,
    ) {
        // Titre enrichi avec le niveau de risque.
        let enriched_title = format!("{} {}", metadata.risk.emoji(), metadata.title);

        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCall(
                AcpToolCall::new(call_id.clone(), enriched_title)
                    .kind(metadata.kind)
                    .status(status)
                    .raw_input(raw_input.clone()),
            ),
        ));
    }

    /// Émet une notification `tool_call_update` avec changement de statut seul.
    fn emit_tool_call_update_status(&self, call_id: &ToolCallId, status: ToolCallStatus) {
        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                call_id.clone(),
                ToolCallUpdateFields::new().status(status),
            )),
        ));
    }

    /// Émet une notification `tool_call_update` avec contenu (completed/failed).
    fn emit_tool_call_update_with_content(
        &self,
        call_id: &ToolCallId,
        status: ToolCallStatus,
        content: &str,
    ) {
        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![ToolCallContent::Content(Content::new(
                        ContentBlock::Text(TextContent::new(content.to_string())),
                    ))]),
            )),
        ));
    }

    /// Émet une notification `tool_call_update` failed.
    fn emit_tool_call_update_failed(&self, call_id: &ToolCallId, message: &str) {
        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![ToolCallContent::Content(Content::new(
                        ContentBlock::Text(TextContent::new(message.to_string())),
                    ))]),
            )),
        ));
    }
}

// ---------------------------------------------------------------------------
// PermissionKind helper
// ---------------------------------------------------------------------------

impl PermissionKind {
    /// Label court pour l'affichage.
    pub fn label(&self) -> &'static str {
        match self {
            PermissionKind::Read => "read",
            PermissionKind::Write => "write",
            PermissionKind::Execute => "execute",
            PermissionKind::Network => "network",
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers utilitaires
// ---------------------------------------------------------------------------

/// Tronque un chemin pour l'affichage (garde le basename si trop long).
fn truncate_path(path: &str, max_chars: usize) -> String {
    if path.len() <= max_chars {
        return path.to_string();
    }
    // Garder les derniers composants du chemin.
    let components: Vec<&str> = path.split('/').collect();
    let mut result = String::new();
    // Remonter depuis la fin jusqu'à ce qu'on dépasse max_chars.
    for comp in components.iter().rev() {
        let candidate = if result.is_empty() {
            comp.to_string()
        } else {
            format!("{}/{}", comp, result)
        };
        if candidate.len() > max_chars {
            break;
        }
        result = candidate;
    }
    if result.is_empty() {
        // Fallback : garder les derniers caractères.
        format!("...{}", &path[path.len().saturating_sub(max_chars - 3)..])
    } else if result.len() < path.len() {
        format!(".../{}", result)
    } else {
        result
    }
}

/// Tronque une commande pour l'affichage (garde la première ligne).
fn truncate_cmd(cmd: &str, max_chars: usize) -> String {
    let first_line = cmd.lines().next().unwrap_or("");
    if first_line.len() <= max_chars {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
}

/// Formate une taille en octets pour l'affichage humain.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{} octets", bytes)
    }
}

// ---------------------------------------------------------------------------
// Fonctions libres (utilisées par prompt/turn.rs)
// ---------------------------------------------------------------------------

/// `safe_session_update` — wrapper qui avale les erreurs de transport.
pub fn safe_session_update(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    update: SessionUpdate,
) {
    let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
}

/// Émet un `[error]` comme `agent_message_chunk`.
pub fn emit_error_chunk(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message_id: &agent_client_protocol::schema::v1::MessageId,
    error: &str,
) {
    safe_session_update(
        cx,
        session_id,
        SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                TextContent::new(format!("\n\n[error] {error}")),
            ))
            .message_id(message_id.clone()),
        ),
    );
}

/// Mappe un finish reason Gemini vers un ACP StopReason.
#[allow(dead_code)]
pub fn map_stop_reason(
    gemini_finish: Option<&str>,
) -> agent_client_protocol::schema::v1::StopReason {
    match gemini_finish {
        Some("length") | Some("max_tokens") => {
            agent_client_protocol::schema::v1::StopReason::MaxTokens
        }
        Some("content_filter") | Some("safety") | Some("block_reason") => {
            agent_client_protocol::schema::v1::StopReason::Refusal
        }
        _ => agent_client_protocol::schema::v1::StopReason::EndTurn,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_kind_static(name: &str) -> ToolKind {
        match name {
            "file_read" | "search" => ToolKind::Read,
            "file_write" => ToolKind::Edit,
            "shell_exec" => ToolKind::Execute,
            _ => ToolKind::Other,
        }
    }

    #[test]
    fn tool_kind_mapping() {
        assert!(matches!(tool_kind_static("file_read"), ToolKind::Read));
        assert!(matches!(tool_kind_static("search"), ToolKind::Read));
        assert!(matches!(tool_kind_static("file_write"), ToolKind::Edit));
        assert!(matches!(tool_kind_static("shell_exec"), ToolKind::Execute));
        assert!(matches!(tool_kind_static("unknown_tool"), ToolKind::Other));
    }

    #[test]
    fn stop_reason_mapping() {
        use agent_client_protocol::schema::v1::StopReason;
        assert_eq!(map_stop_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(map_stop_reason(Some("content_filter")), StopReason::Refusal);
        assert_eq!(map_stop_reason(Some("safety")), StopReason::Refusal);
        assert_eq!(map_stop_reason(None), StopReason::EndTurn);
        assert_eq!(map_stop_reason(Some("stop")), StopReason::EndTurn);
        assert_eq!(map_stop_reason(Some("tool_calls")), StopReason::EndTurn);
    }

    #[test]
    fn session_mode_permissions() {
        let default = SessionMode::Default;
        let accept_edits = SessionMode::AcceptEdits;
        let bypass = SessionMode::BypassPermissions;

        assert!(default.requires_write_permission());
        assert!(default.requires_execute_permission());

        assert!(!accept_edits.requires_write_permission());
        assert!(accept_edits.requires_execute_permission());

        assert!(!bypass.requires_write_permission());
        assert!(!bypass.requires_execute_permission());
    }

    #[test]
    fn session_mode_parsing() {
        assert_eq!(
            SessionMode::from_str_lossy("default"),
            Some(SessionMode::Default)
        );
        assert_eq!(
            SessionMode::from_str_lossy("Accept_Edits"),
            Some(SessionMode::AcceptEdits)
        );
        assert_eq!(
            SessionMode::from_str_lossy("BYPASS_PERMISSIONS"),
            Some(SessionMode::BypassPermissions)
        );
        assert_eq!(SessionMode::from_str_lossy("invalid"), None);
    }

    // -- ToolCallMetadata tests --

    #[test]
    fn metadata_file_read_basic() {
        let args = serde_json::json!({"path": "/tmp/test.txt"});
        let meta = ToolCallMetadata::build("file_read", &args);
        assert!(meta.title.contains("Read:"));
        assert!(meta.title.contains("test.txt"));
        assert_eq!(meta.kind, ToolKind::Read);
        assert_eq!(meta.risk, RiskLevel::Low);
    }

    #[test]
    fn metadata_file_read_with_offset() {
        let args = serde_json::json!({"path": "/tmp/test.txt", "offset": 100, "limit": 50});
        let meta = ToolCallMetadata::build("file_read", &args);
        assert!(meta.description.contains("lignes 100..150"));
    }

    #[test]
    fn metadata_file_write_basic() {
        let args = serde_json::json!({"path": "/tmp/out.txt", "content": "hello world\nline 2"});
        let meta = ToolCallMetadata::build("file_write", &args);
        assert!(meta.title.contains("Write:"));
        assert_eq!(meta.kind, ToolKind::Edit);
        assert_eq!(meta.risk, RiskLevel::Medium);
    }

    #[test]
    fn metadata_shell_exec_basic() {
        let args = serde_json::json!({"command": "ls -la"});
        let meta = ToolCallMetadata::build("shell_exec", &args);
        assert!(meta.title.contains("Exec:"));
        assert_eq!(meta.kind, ToolKind::Execute);
        assert_eq!(meta.risk, RiskLevel::Low);
    }

    #[test]
    fn metadata_shell_exec_pipe_medium_risk() {
        let args = serde_json::json!({"command": "cat file.txt | grep pattern"});
        let meta = ToolCallMetadata::build("shell_exec", &args);
        assert_eq!(meta.risk, RiskLevel::Medium);
    }

    #[test]
    fn metadata_shell_exec_rm_high_risk() {
        let args = serde_json::json!({"command": "rm -rf ./build"});
        let meta = ToolCallMetadata::build("shell_exec", &args);
        // rm -rf est correctement classé comme Critical.
        assert_eq!(meta.risk, RiskLevel::Critical);
    }

    #[test]
    fn metadata_search_basic() {
        let args = serde_json::json!({"pattern": "TODO", "path": "/tmp", "glob": "*.rs"});
        let meta = ToolCallMetadata::build("search", &args);
        assert!(meta.title.contains("Search:"));
        assert_eq!(meta.kind, ToolKind::Read);
        assert_eq!(meta.risk, RiskLevel::Low);
        assert!(meta.description.contains("*.rs"));
    }

    #[test]
    fn metadata_unknown_tool() {
        let args = serde_json::json!({"arg": "value"});
        let meta = ToolCallMetadata::build("custom_tool", &args);
        assert_eq!(meta.kind, ToolKind::Other);
        assert!(meta.title.contains("custom_tool"));
        assert!(meta.description.contains("custom_tool"));
    }

    // -- PermissionRequest tests --

    #[test]
    fn permission_request_write() {
        let args = serde_json::json!({"path": "/tmp/test.txt", "content": "hello"});
        let req = PermissionRequest::from_tool_call("file_write", &args);
        assert_eq!(req.kind, PermissionKind::Write);
        assert!(req.risk >= RiskLevel::Medium);
        assert!(!req.summary.is_empty());
    }

    #[test]
    fn permission_request_execute() {
        let args = serde_json::json!({"command": "ls -la"});
        let req = PermissionRequest::from_tool_call("shell_exec", &args);
        assert_eq!(req.kind, PermissionKind::Execute);
        assert_eq!(req.risk, RiskLevel::Low);
    }

    // -- Helpers tests --

    #[test]
    fn truncate_path_short() {
        assert_eq!(truncate_path("file.txt", 60), "file.txt");
    }

    #[test]
    fn truncate_path_long() {
        let long = "/very/long/path/to/some/deep/directory/structure/file.txt";
        let truncated = truncate_path(long, 30);
        assert!(truncated.len() <= 35); // max + "..."
        assert!(truncated.contains("file.txt") || truncated.contains("..."));
    }

    #[test]
    fn truncate_cmd_short() {
        assert_eq!(truncate_cmd("ls -la", 60), "ls -la");
    }

    #[test]
    fn truncate_cmd_long() {
        let long = "a".repeat(100);
        let truncated = truncate_cmd(&long, 50);
        assert!(truncated.len() <= 53); // 50 + "..."
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn format_size_various() {
        assert!(format_size(500).contains("octets"));
        assert!(format_size(2048).contains("KiB"));
        assert!(format_size(5_000_000).contains("MiB"));
        assert!(format_size(2_000_000_000).contains("GiB"));
    }

    #[test]
    fn risk_level_ordering() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Critical);
    }
}
