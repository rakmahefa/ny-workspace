//! Tool execution and real ACP permission handling.
//!
//! Permission requests are sent to the ACP client with `session/request_permission`.
//! The old local oneshot/timeout broker intentionally does not exist anymore: in
//! `default` mode a mutating tool is blocked until the client answers.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, SessionId, SessionNotification, SessionUpdate, TextContent,
    ToolCall as AcpToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo};

use crate::state::SessionMode;
use super::registry::ToolRegistry;
use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};

/// Human-readable metadata used both by tool_call notifications and permission UI.
#[derive(Debug, Clone)]
pub struct ToolCallMetadata {
    pub title: String,
    pub description: String,
    pub risk: RiskLevel,
    pub kind: ToolKind,
}

impl ToolCallMetadata {
    pub fn build(tool_name: &str, arguments: &serde_json::Value) -> Self {
        match tool_name {
            "file_read" => Self::file_read(arguments),
            "file_write" => Self::file_write(arguments),
            "shell_exec" => Self::shell_exec(arguments),
            "search" => Self::search(arguments),
            _ => Self {
                title: tool_name.to_string(),
                description: format!(
                    "Outil : {}\nArguments : {}",
                    tool_name,
                    serde_json::to_string_pretty(arguments).unwrap_or_else(|_| arguments.to_string())
                ),
                risk: RiskLevel::Medium,
                kind: ToolKind::Other,
            },
        }
    }

    fn file_read(args: &serde_json::Value) -> Self {
        let path = arg_str(args, "path").unwrap_or("<path manquant>");
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(500);
        let mut description = format!("Lecture du fichier : {}", path);
        if offset > 0 || limit < 500 {
            description.push_str(&format!(" (lignes {}..{}, max {})", offset, offset + limit, limit));
        }
        if let Ok(metadata) = std::fs::metadata(path) {
            description.push_str(&format!("\nTaille : {}", format_size(metadata.len())));
        }
        Self {
            title: format!("Read: {}", truncate_path(path, 60)),
            description,
            risk: RiskLevel::Low,
            kind: ToolKind::Read,
        }
    }

    fn file_write(args: &serde_json::Value) -> Self {
        let path = arg_str(args, "path").unwrap_or("<path manquant>");
        let content = arg_str(args, "content").unwrap_or("");
        let action = if std::fs::metadata(path).is_ok() { "Modification" } else { "Création" };
        Self {
            title: format!("Write: {}", truncate_path(path, 60)),
            description: format!(
                "{} du fichier : {}\nTaille : {} ({} octets)\nLignes : {}",
                action, path, format_size(content.len() as u64), content.len(), content.lines().count()
            ),
            risk: RiskLevel::Medium,
            kind: ToolKind::Edit,
        }
    }

    fn shell_exec(args: &serde_json::Value) -> Self {
        let command = arg_str(args, "command").unwrap_or("<commande manquante>");
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
        match ShellSandbox::new().analyze_command(command) {
            Ok(analysis) => Self {
                title: format!("Exec: {}", truncate_cmd(command, 60)),
                description: format!(
                    "{}\nRisque : {} {}\nTimeout : {}s\n{}",
                    analysis.summary(), analysis.risk.emoji(), analysis.risk.label(), timeout,
                    analysis.risk_description
                ),
                risk: analysis.risk,
                kind: ToolKind::Execute,
            },
            Err(error) => Self {
                title: format!("Exec: {}", truncate_cmd(command, 60)),
                description: format!("Commande bloquée par la sandbox : {}\n{}", command, error),
                risk: RiskLevel::Critical,
                kind: ToolKind::Execute,
            },
        }
    }

    fn search(args: &serde_json::Value) -> Self {
        let pattern = arg_str(args, "pattern").unwrap_or("<pattern manquant>");
        let path = arg_str(args, "path").unwrap_or("CWD");
        let glob = arg_str(args, "glob").unwrap_or("all files");
        Self {
            title: format!("Search: {}", truncate_cmd(pattern, 60)),
            description: format!("Recherche : '{}' dans {}\nFiltre : {}", pattern, path, glob),
            risk: RiskLevel::Low,
            kind: ToolKind::Read,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub kind: PermissionKind,
    pub risk: RiskLevel,
    pub summary: String,
    pub detail: String,
    pub tool_name: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionKind {
    Read,
    Write,
    Execute,
    #[allow(dead_code)]
    Network,
}

impl PermissionRequest {
    pub fn from_tool_call(tool_name: &str, args: &serde_json::Value) -> Self {
        let metadata = ToolCallMetadata::build(tool_name, args);
        let kind = match tool_name {
            "file_read" | "search" => PermissionKind::Read,
            "file_write" => PermissionKind::Write,
            _ => PermissionKind::Execute,
        };
        let mut warnings = Vec::new();
        match tool_name {
            "file_write" => {
                if let Some(path) = arg_str(args, "path") {
                    if std::fs::metadata(path).is_ok() {
                        warnings.push(format!("Le fichier '{}' existe déjà et sera modifié.", path));
                    }
                }
            }
            "shell_exec" => {
                let command = arg_str(args, "command").unwrap_or("");
                let analysis = ShellAnalysis::analyze(command);
                if analysis.has_dangerous_pipe_chain {
                    warnings.push("Chaîne de commandes potentiellement dangereuse détectée.".into());
                }
                if analysis.has_env_injection {
                    warnings.push("Injection de variables d'environnement détectée.".into());
                }
                if analysis.risk >= RiskLevel::High {
                    warnings.push(format!("Niveau de risque {} : {}", analysis.risk.emoji(), analysis.risk.description()));
                }
            }
            _ => {}
        }
        if metadata.risk >= RiskLevel::High {
            warnings.push("Cette opération peut avoir des effets irréversibles.".into());
        }
        let warning_text = if warnings.is_empty() {
            String::new()
        } else {
            format!("\nAvertissements :\n{}", warnings.iter().map(|w| format!("  - {w}")).collect::<Vec<_>>().join("\n"))
        };
        Self {
            kind,
            risk: metadata.risk,
            summary: metadata.title,
            detail: format!("{}\n{} {}{}", metadata.description, metadata.risk.emoji(), metadata.risk.label(), warning_text),
            tool_name: tool_name.to_string(),
            warnings,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    #[allow(dead_code)]
    pub is_ok: bool,
}

impl ToolResult {
    #[allow(dead_code)]
    pub fn ok(content: impl Into<String>) -> Self { Self { content: content.into(), is_ok: true } }
    pub fn err(content: impl Into<String>) -> Self { Self { content: content.into(), is_ok: false } }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult {
    Allow,
    #[allow(dead_code)]
    Reject,
    Cancelled,
    #[allow(dead_code)]
    TransportError(String),
}

/// Central tool executor. ACP permission requests are sent directly through `ConnectionTo<Client>`.
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
        Self { cx, session_id, registry, cwd, additional_dirs, get_mode }
    }

    pub async fn execute(&self, tool_name: &str, arguments: &serde_json::Value) -> ToolResult {
        let call_id = ToolCallId::from(format!("call_{}", uuid::Uuid::new_v4().simple()));
        let metadata = ToolCallMetadata::build(tool_name, arguments);
        let mode = (self.get_mode)();
        let needs_permission = match metadata.kind {
            ToolKind::Edit | ToolKind::Execute => !matches!(mode, SessionMode::BypassPermissions),
            ToolKind::Read => false,
            _ => metadata.risk >= RiskLevel::High && matches!(mode, SessionMode::AcceptEdits),
        };

        self.emit_tool_call(&call_id, &metadata, if needs_permission { ToolCallStatus::Pending } else { ToolCallStatus::InProgress }, arguments);

        if needs_permission {
            let request = PermissionRequest::from_tool_call(tool_name, arguments);
            match self.request_permission(&call_id, &request).await {
                PermissionResult::Allow => self.emit_tool_call_update_status(&call_id, ToolCallStatus::InProgress),
                PermissionResult::Reject => {
                    let message = format!("{} ({}) refusé par l'utilisateur.", request.kind.label(), request.summary);
                    self.emit_tool_call_update_failed(&call_id, &message);
                    return ToolResult::err(message);
                }
                PermissionResult::Cancelled => {
                    let message = format!("{} ({}) annulé par l'utilisateur.", request.kind.label(), request.summary);
                    self.emit_tool_call_update_failed(&call_id, &message);
                    return ToolResult::err(message);
                }
                PermissionResult::TransportError(error) => {
                    let message = format!("Échec de la demande de permission ACP : {error}");
                    self.emit_tool_call_update_failed(&call_id, &message);
                    return ToolResult::err(message);
                }
            }
        }

        match self.registry.call_async(tool_name, arguments, self.cwd, self.additional_dirs).await {
            Some(result) => {
                let status = if result.is_ok() { ToolCallStatus::Completed } else { ToolCallStatus::Failed };
                let text = result.to_history_text();
                self.emit_tool_call_update_with_content(&call_id, status, &text);
                ToolResult { content: text, is_ok: result.is_ok() }
            }
            None => {
                let message = format!("Unknown tool: {tool_name}");
                tracing::warn!(session = %self.session_id, tool = %tool_name, "outil inconnu");
                self.emit_tool_call_update_failed(&call_id, &message);
                ToolResult::err(message)
            }
        }
    }

    /// Send the standard ACP `session/request_permission` request and wait for the client's response.
    ///
    /// The request is made from the spawned prompt task, not from the ACP dispatch loop, so
    /// `block_task()` is safe and does not deadlock the connection.
    pub async fn request_permission(&self, call_id: &ToolCallId, request: &PermissionRequest) -> PermissionResult {
        let tool_call = AcpToolCall::new(call_id.clone(), request.summary.clone())
            .kind(match request.kind {
                PermissionKind::Read => ToolKind::Read,
                PermissionKind::Write => ToolKind::Edit,
                PermissionKind::Execute => ToolKind::Execute,
                PermissionKind::Network => ToolKind::Fetch,
            })
            .status(ToolCallStatus::Pending);

        let options = vec![
            PermissionOption::new("allow_once", "Autoriser cette fois", PermissionOptionKind::AllowOnce),
            PermissionOption::new("allow_always", "Toujours autoriser", PermissionOptionKind::AllowAlways),
            PermissionOption::new("reject_once", "Refuser", PermissionOptionKind::RejectOnce),
        ];
        let rpc = RequestPermissionRequest::new(self.session_id.clone(), ToolCallUpdate::from(tool_call), options);

        tracing::info!(
            session = %self.session_id,
            tool = %request.tool_name,
            kind = ?request.kind,
            risk = %request.risk,
            summary = %request.summary,
            detail = %request.detail,
            warnings = ?request.warnings,
            "envoi session/request_permission"
        );

        let response = match self.cx.send_request(rpc).block_task().await {
            Ok(response) => response,
            Err(error) => return PermissionResult::TransportError(error.to_string()),
        };

        match response.outcome {
            RequestPermissionOutcome::Cancelled => PermissionResult::Cancelled,
            RequestPermissionOutcome::Selected(selected) => {
                match selected.option_id.0.as_ref() {
                    "allow_once" | "allow_always" => {
                        tracing::info!(session = %self.session_id, tool = %request.tool_name, option = %selected.option_id, "permission ACP accordée");
                        PermissionResult::Allow
                    }
                    "reject_once" | "reject_always" => {
                        tracing::info!(session = %self.session_id, tool = %request.tool_name, option = %selected.option_id, "permission ACP refusée");
                        PermissionResult::Reject
                    }
                    unknown => PermissionResult::TransportError(format!("option de permission ACP inconnue: {unknown}")),
                }
            }
            _ => PermissionResult::TransportError("outcome de permission ACP non reconnu".into()),
        }
    }

    fn emit_tool_call(&self, id: &ToolCallId, metadata: &ToolCallMetadata, status: ToolCallStatus, raw_input: &serde_json::Value) {
        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCall(
                AcpToolCall::new(id.clone(), format!("{} {}", metadata.risk.emoji(), metadata.title))
                    .kind(metadata.kind)
                    .status(status)
                    .raw_input(raw_input.clone()),
            ),
        ));
    }

    fn emit_tool_call_update_status(&self, id: &ToolCallId, status: ToolCallStatus) {
        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id.clone(), ToolCallUpdateFields::new().status(status))),
        ));
    }

    fn emit_tool_call_update_with_content(&self, id: &ToolCallId, status: ToolCallStatus, content: &str) {
        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.clone(),
                ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(content.to_string()))))]),
            )),
        ));
    }

    fn emit_tool_call_update_failed(&self, id: &ToolCallId, message: &str) {
        self.emit_tool_call_update_with_content(id, ToolCallStatus::Failed, message);
    }
}

impl PermissionKind {
    pub fn label(&self) -> &'static str {
        match self {
            PermissionKind::Read => "read",
            PermissionKind::Write => "write",
            PermissionKind::Execute => "execute",
            PermissionKind::Network => "network",
        }
    }
}

fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

fn truncate_path(path: &str, max_chars: usize) -> String {
    if path.len() <= max_chars { return path.to_string(); }
    let tail = path.rsplit('/').take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
    if tail.len() + 4 <= max_chars { format!(".../{tail}") } else { format!("...{}", &path[path.len().saturating_sub(max_chars.saturating_sub(3))..]) }
}

fn truncate_cmd(cmd: &str, max_chars: usize) -> String {
    let line = cmd.lines().next().unwrap_or("");
    if line.len() <= max_chars { line.to_string() } else { format!("{}...", line.chars().take(max_chars).collect::<String>()) }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB { format!("{:.1} GiB", bytes as f64 / GB as f64) }
    else if bytes >= MB { format!("{:.1} MiB", bytes as f64 / MB as f64) }
    else if bytes >= KB { format!("{:.1} KiB", bytes as f64 / KB as f64) }
    else { format!("{} octets", bytes) }
}

pub fn safe_session_update(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) {
    let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
}

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
            agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(TextContent::new(format!("\n\n[error] {error}"))))
                .message_id(message_id.clone()),
        ),
    );
}

#[allow(dead_code)]
pub fn map_stop_reason(gemini_finish: Option<&str>) -> agent_client_protocol::schema::v1::StopReason {
    match gemini_finish {
        Some("length") | Some("max_tokens") => agent_client_protocol::schema::v1::StopReason::MaxTokens,
        Some("content_filter") | Some("safety") | Some("block_reason") => agent_client_protocol::schema::v1::StopReason::Refusal,
        _ => agent_client_protocol::schema::v1::StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kind_mapping() {
        assert_eq!(ToolCallMetadata::build("file_read", &serde_json::json!({})).kind, ToolKind::Read);
        assert_eq!(ToolCallMetadata::build("search", &serde_json::json!({})).kind, ToolKind::Read);
        assert_eq!(ToolCallMetadata::build("file_write", &serde_json::json!({})).kind, ToolKind::Edit);
        assert_eq!(ToolCallMetadata::build("shell_exec", &serde_json::json!({"command":"ls"})).kind, ToolKind::Execute);
    }

    #[test]
    fn permission_request_write() {
        let request = PermissionRequest::from_tool_call("file_write", &serde_json::json!({"path":"/tmp/test","content":"hello"}));
        assert_eq!(request.kind, PermissionKind::Write);
        assert!(!request.summary.is_empty());
    }

    #[test]
    fn permission_request_execute() {
        let request = PermissionRequest::from_tool_call("shell_exec", &serde_json::json!({"command":"ls -la"}));
        assert_eq!(request.kind, PermissionKind::Execute);
        assert_eq!(request.risk, RiskLevel::Low);
    }

    #[test]
    fn stop_reason_mapping() {
        use agent_client_protocol::schema::v1::StopReason;
        assert_eq!(map_stop_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(map_stop_reason(Some("content_filter")), StopReason::Refusal);
        assert_eq!(map_stop_reason(None), StopReason::EndTurn);
    }
}
