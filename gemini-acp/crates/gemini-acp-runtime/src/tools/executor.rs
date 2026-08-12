//! Tool execution, ACP permissions, and rich tool-call UX.
//!
//! The executor keeps the permission flow real (`session/request_permission`)
//! while exposing the tool lifecycle in a form clients can render usefully:
//! file locations, diffs for writes, concise titles, and bounded raw input.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, Diff, PermissionOption, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, TextContent, ToolCall as AcpToolCall, ToolCallContent, ToolCallId,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo};

use crate::state::SessionMode;
use super::registry::ToolRegistry;
use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};

const MAX_RAW_INPUT_CHARS: usize = 8_192;
const MAX_DIFF_OLD_TEXT_BYTES: u64 = 64 * 1024;

/// Human-readable and structured metadata shared by tool-call creation and
/// permission dialogs.
#[derive(Debug, Clone)]
pub struct ToolCallMetadata {
    pub title: String,
    pub description: String,
    pub risk: RiskLevel,
    pub kind: ToolKind,
    pub content: Vec<ToolCallContent>,
    pub locations: Vec<ToolCallLocation>,
}

impl ToolCallMetadata {
    pub fn build(tool_name: &str, arguments: &serde_json::Value, cwd: &Path) -> Self {
        match tool_name {
            "file_read" => Self::file_read(arguments, cwd),
            "file_write" => Self::file_write(arguments, cwd),
            "shell_exec" => Self::shell_exec(arguments),
            "search" => Self::search(arguments, cwd),
            _ => Self {
                title: tool_name.to_string(),
                description: concise_generic_description(tool_name, arguments),
                risk: RiskLevel::Medium,
                kind: ToolKind::Other,
                content: vec![],
                locations: vec![],
            },
        }
    }

    fn file_read(args: &serde_json::Value, cwd: &Path) -> Self {
        let path = arg_str(args, "path").unwrap_or("<path manquant>");
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(500);
        let display = display_path(path, cwd);
        let mut description = format!("Lecture du fichier : {display}");
        if offset > 0 || limit < 500 {
            description.push_str(&format!(" (lignes {}..{}, max {})", offset, offset + limit, limit));
        }
        if let Ok(metadata) = std::fs::metadata(resolve_path(path, cwd)) {
            description.push_str(&format!("\nTaille : {}", format_size(metadata.len())));
        }
        Self {
            title: if offset > 0 {
                format!("Read {display} @ {}", offset)
            } else {
                format!("Read {display}")
            },
            description,
            risk: RiskLevel::Low,
            kind: ToolKind::Read,
            content: vec![],
            locations: vec![ToolCallLocation::new(resolve_path(path, cwd)).line(offset.max(1) as u32)],
        }
    }

    fn file_write(args: &serde_json::Value, cwd: &Path) -> Self {
        let path = arg_str(args, "path").unwrap_or("<path manquant>");
        let content = arg_str(args, "content").unwrap_or("");
        let resolved = resolve_path(path, cwd);
        let display = display_path(path, cwd);
        let action = if resolved.exists() { "Modification" } else { "Création" };
        let old_text = read_old_text(&resolved);
        let diff = Diff::new(resolved.clone(), content.to_string()).old_text(old_text);
        Self {
            title: format!("Write {display}"),
            description: format!(
                "{action} du fichier : {display}\nTaille : {} ({} octets)\nLignes : {}",
                format_size(content.len() as u64), content.len(), content.lines().count()
            ),
            risk: RiskLevel::Medium,
            kind: ToolKind::Edit,
            content: vec![ToolCallContent::Diff(diff)],
            locations: vec![ToolCallLocation::new(resolved)],
        }
    }

    fn shell_exec(args: &serde_json::Value) -> Self {
        let command = arg_str(args, "command").unwrap_or("<commande manquante>");
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
        match ShellSandbox::new().analyze_command(command) {
            Ok(analysis) => Self {
                title: format!("Run {}", truncate_cmd(command, 72)),
                description: format!(
                    "{}\nRisque : {} {}\nTimeout : {}s\n{}",
                    analysis.summary(), analysis.risk.emoji(), analysis.risk.label(), timeout,
                    analysis.risk_description
                ),
                risk: analysis.risk,
                kind: ToolKind::Execute,
                content: vec![],
                locations: vec![],
            },
            Err(error) => Self {
                title: format!("Run {}", truncate_cmd(command, 72)),
                description: format!("Commande bloquée par la sandbox : {command}\n{error}"),
                risk: RiskLevel::Critical,
                kind: ToolKind::Execute,
                content: vec![],
                locations: vec![],
            },
        }
    }

    fn search(args: &serde_json::Value, cwd: &Path) -> Self {
        let pattern = arg_str(args, "pattern").unwrap_or("<pattern manquant>");
        let path = arg_str(args, "path").unwrap_or(".");
        let glob = arg_str(args, "glob").unwrap_or("all files");
        let resolved = resolve_path(path, cwd);
        let display = display_path(path, cwd);
        Self {
            title: format!("Search '{}'{}", truncate_cmd(pattern, 56), if display == "." { String::new() } else { format!(" in {display}") }),
            description: format!("Recherche : '{}' dans {}\nFiltre : {}", pattern, display, glob),
            risk: RiskLevel::Low,
            kind: ToolKind::Read,
            content: vec![],
            locations: vec![ToolCallLocation::new(resolved)],
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
    pub fn from_tool_call(tool_name: &str, args: &serde_json::Value, cwd: &Path) -> Self {
        let metadata = ToolCallMetadata::build(tool_name, args, cwd);
        let kind = match tool_name {
            "file_read" | "search" => PermissionKind::Read,
            "file_write" => PermissionKind::Write,
            _ => PermissionKind::Execute,
        };
        let mut warnings = Vec::new();
        match tool_name {
            "file_write" => {
                if let Some(path) = arg_str(args, "path") {
                    let resolved = resolve_path(path, cwd);
                    if resolved.exists() {
                        warnings.push(format!("Le fichier '{}' existe déjà et sera modifié.", display_path(path, cwd)));
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
        let metadata = ToolCallMetadata::build(tool_name, arguments, self.cwd);
        let mode = (self.get_mode)();
        let needs_permission = match metadata.kind {
            ToolKind::Edit | ToolKind::Execute => !matches!(mode, SessionMode::BypassPermissions),
            ToolKind::Read => false,
            _ => metadata.risk >= RiskLevel::High && matches!(mode, SessionMode::AcceptEdits),
        };

        self.emit_tool_call(&call_id, &metadata, if needs_permission { ToolCallStatus::Pending } else { ToolCallStatus::InProgress }, arguments);

        if needs_permission {
            let request = PermissionRequest::from_tool_call(tool_name, arguments, self.cwd);
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
            RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
                "allow_once" | "allow_always" => PermissionResult::Allow,
                "reject_once" | "reject_always" => PermissionResult::Reject,
                unknown => PermissionResult::TransportError(format!("option de permission ACP inconnue: {unknown}")),
            },
            _ => PermissionResult::TransportError("outcome de permission ACP non reconnu".into()),
        }
    }

    fn emit_tool_call(&self, id: &ToolCallId, metadata: &ToolCallMetadata, status: ToolCallStatus, raw_input: &serde_json::Value) {
        let safe_input = bounded_raw_input(raw_input);
        let call = AcpToolCall::new(id.clone(), format!("{} {}", metadata.risk.emoji(), metadata.title))
            .kind(metadata.kind)
            .status(status)
            .content(metadata.content.clone())
            .locations(metadata.locations.clone())
            .raw_input(safe_input);
        let _ = self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCall(call),
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

fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() { candidate } else { cwd.join(candidate) }
}

fn display_path(path: &str, cwd: &Path) -> String {
    let resolved = resolve_path(path, cwd);
    match resolved.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        Ok(_) => ".".into(),
        Err(_) => path.to_string(),
    }
}

fn read_old_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_DIFF_OLD_TEXT_BYTES { return None; }
    std::fs::read_to_string(path).ok()
}

fn bounded_raw_input(input: &serde_json::Value) -> serde_json::Value {
    let mut value = input.clone();
    if let Some(object) = value.as_object_mut() {
        if let Some(content) = object.get_mut("content").as_deref().and_then(|arg0: &serde_json::Value| serde_json::Value::as_str(&*arg0)) {
            if content.chars().count() > MAX_RAW_INPUT_CHARS {
                let preview: String = content.chars().take(MAX_RAW_INPUT_CHARS).collect();
                *object.get_mut("content").expect("content exists") = serde_json::Value::String(format!(
                    "{}\n… [{} chars omitted from ACP display]",
                    preview,
                    content.chars().count() - MAX_RAW_INPUT_CHARS
                ));
            }
        }
    }
    value
}

fn concise_generic_description(tool_name: &str, args: &serde_json::Value) -> String {
    let keys = args.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    if keys.is_empty() {
        format!("Outil : {tool_name}")
    } else {
        format!("Outil : {tool_name}\nArguments : {}", keys.join(", "))
    }
}

fn truncate_cmd(cmd: &str, max_chars: usize) -> String {
    let line = cmd.lines().next().unwrap_or("");
    if line.chars().count() <= max_chars { line.to_string() } else { format!("{}…", line.chars().take(max_chars).collect::<String>()) }
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
            agent_client_protocol::schema::v1::ContentChunk::new(
                ContentBlock::Text(TextContent::new(format!("\n\n[error] {error}"))),
            )
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
        let cwd = Path::new("/tmp/project");
        assert_eq!(ToolCallMetadata::build("file_read", &serde_json::json!({"path":"src/main.rs"}), cwd).kind, ToolKind::Read);
        assert_eq!(ToolCallMetadata::build("search", &serde_json::json!({"pattern":"TODO"}), cwd).kind, ToolKind::Read);
        assert_eq!(ToolCallMetadata::build("file_write", &serde_json::json!({"path":"src/main.rs","content":"fn main() {}"}), cwd).kind, ToolKind::Edit);
        assert_eq!(ToolCallMetadata::build("shell_exec", &serde_json::json!({"command":"ls"}), cwd).kind, ToolKind::Execute);
    }

    #[test]
    fn file_write_exposes_diff_and_location() {
        let cwd = Path::new("/tmp/project");
        let metadata = ToolCallMetadata::build("file_write", &serde_json::json!({"path":"src/lib.rs","content":"hello"}), cwd);
        assert!(matches!(metadata.content.first(), Some(ToolCallContent::Diff(_))));
        assert_eq!(metadata.locations.len(), 1);
        assert_eq!(metadata.title, "Write src/lib.rs");
    }

    #[test]
    fn display_path_is_project_relative() {
        let cwd = Path::new("/tmp/project");
        assert_eq!(display_path("src/lib.rs", cwd), "src/lib.rs");
        assert_eq!(display_path("/tmp/outside.rs", cwd), "/tmp/outside.rs");
    }

    #[test]
    fn raw_input_is_bounded_for_large_writes() {
        let input = serde_json::json!({"path":"x","content":"a".repeat(MAX_RAW_INPUT_CHARS + 32)});
        let bounded = bounded_raw_input(&input);
        let content = bounded.get("content").and_then(|v| v.as_str()).unwrap();
        assert!(content.chars().count() < MAX_RAW_INPUT_CHARS + 64);
        assert!(content.contains("chars omitted"));
    }

    #[test]
    fn permission_request_write() {
        let request = PermissionRequest::from_tool_call(
            "file_write",
            &serde_json::json!({"path":"/tmp/test","content":"hello"}),
            Path::new("/tmp"),
        );
        assert_eq!(request.kind, PermissionKind::Write);
        assert!(!request.summary.is_empty());
    }

    #[test]
    fn stop_reason_mapping() {
        use agent_client_protocol::schema::v1::StopReason;
        assert_eq!(map_stop_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(map_stop_reason(Some("content_filter")), StopReason::Refusal);
        assert_eq!(map_stop_reason(None), StopReason::EndTurn);
    }
}
