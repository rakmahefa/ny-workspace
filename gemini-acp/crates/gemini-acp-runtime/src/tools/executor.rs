//! Tool executor with Claude-style ACP UX and real ACP permissions.
//!
//! The execution pipeline intentionally mirrors the separation used by
//! `agentclientprotocol/claude-agent-acp/src/tools.ts`:
//!
//! 1. build a structured tool presentation (title/kind/content/locations),
//! 2. announce `tool_call`,
//! 3. request ACP permission when policy requires it,
//! 4. execute the tool (terminal UX is delegated to ACP when available),
//! 5. render the result with a tool-specific ACP update,
//! 6. finish with `completed` / `failed`.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, CreateTerminalRequest, PermissionOption, PermissionOptionKind,
    ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    SessionNotification, SessionUpdate, Terminal, TerminalOutputRequest, TextContent,
    ToolCall as AcpToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, WaitForTerminalExitRequest,
};
use agent_client_protocol::{Client, ConnectionTo};

use crate::state::SessionMode;

use super::registry::{ToolRegistry, ToolResult as RegistryToolResult};
use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};
use super::tool_ux::{bounded_raw_input, classify_risk, result_update, ToolInfo};

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
        let info = ToolInfo::build(tool_name, args, cwd, None);
        let kind = match info.kind {
            ToolKind::Read | ToolKind::Search => PermissionKind::Read,
            ToolKind::Edit => PermissionKind::Write,
            ToolKind::Execute => PermissionKind::Execute,
            _ => PermissionKind::Execute,
        };

        let risk = classify_risk(tool_name, args);
        let mut warnings = Vec::new();

        match tool_name {
            "file_write" | "file_edit" | "replace_in_file" => {
                if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                    let resolved = if Path::new(path).is_absolute() {
                        PathBuf::from(path)
                    } else {
                        cwd.join(path)
                    };
                    if resolved.exists() {
                        warnings.push(format!("Le fichier '{}' existe déjà et sera modifié.", info.title));
                    }
                }
            }
            "shell_exec" => {
                let command = args.get("command").and_then(serde_json::Value::as_str).unwrap_or("");
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

        if risk >= RiskLevel::High {
            warnings.push("Cette opération peut avoir des effets irréversibles.".into());
        }

        let detail = if warnings.is_empty() {
            format!("{}\n{} {}", info.title, risk.emoji(), risk.label())
        } else {
            format!(
                "{}\n{} {}\n\nAvertissements :\n{}",
                info.title,
                risk.emoji(),
                risk.label(),
                warnings.iter().map(|warning| format!("  - {warning}")).collect::<Vec<_>>().join("\n")
            )
        };

        Self {
            kind,
            risk,
            summary: info.title,
            detail,
            tool_name: tool_name.to_owned(),
            warnings,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_ok: bool,
}

impl ToolResult {
    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_ok: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult {
    Allow,
    Reject,
    Cancelled,
    TransportError(String),
}

#[derive(Debug)]
struct ExecutionOutcome {
    result: ToolResult,
    terminal_id: Option<String>,
}

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
        let info = ToolInfo::build(tool_name, arguments, self.cwd, None);
        let mode = (self.get_mode)();
        let needs_permission = match info.kind {
            ToolKind::Edit | ToolKind::Execute => match mode {
                SessionMode::BypassPermissions => false,
                SessionMode::AcceptEdits => info.kind == ToolKind::Execute && classify_risk(tool_name, arguments) >= RiskLevel::High,
                SessionMode::Default => true,
            },
            _ => false,
        };

        self.emit_tool_call(&call_id, &info, if needs_permission { ToolCallStatus::Pending } else { ToolCallStatus::InProgress }, arguments);

        if needs_permission {
            let request = PermissionRequest::from_tool_call(tool_name, arguments, self.cwd);
            match self.request_permission(&request, &call_id).await {
                PermissionResult::Allow => self.emit_status(&call_id, ToolCallStatus::InProgress),
                PermissionResult::Reject => {
                    let message = format!("{} ({}) refusé par l'utilisateur.", request.kind.label(), request.summary);
                    self.emit_failed(&call_id, &message, arguments, tool_name);
                    return ToolResult::err(message);
                }
                PermissionResult::Cancelled => {
                    let message = format!("{} ({}) annulé par l'utilisateur.", request.kind.label(), request.summary);
                    self.emit_failed(&call_id, &message, arguments, tool_name);
                    return ToolResult::err(message);
                }
                PermissionResult::TransportError(error) => {
                    let message = format!("Échec de la demande de permission ACP : {error}");
                    self.emit_failed(&call_id, &message, arguments, tool_name);
                    return ToolResult::err(message);
                }
            }
        }

        let outcome = if tool_name == "shell_exec" {
            match self.execute_shell_via_acp_terminal(arguments, &call_id).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::debug!(session = %self.session_id, error = %error, "terminal ACP indisponible, fallback shell local");
                    self.execute_registry(tool_name, arguments).await
                }
            }
        } else {
            self.execute_registry(tool_name, arguments).await
        };

        let rendered = result_update(
            tool_name,
            arguments,
            &outcome.result.content,
            outcome.result.is_ok,
            self.cwd,
            outcome.terminal_id.as_deref(),
        );
        self.emit_update(&call_id, rendered.status, rendered.content, rendered.locations);
        outcome.result
    }

    async fn execute_registry(&self, tool_name: &str, arguments: &serde_json::Value) -> ExecutionOutcome {
        match self.registry.call_async(tool_name, arguments, self.cwd, self.additional_dirs).await {
            Some(result) => ExecutionOutcome { result: registry_result(result), terminal_id: None },
            None => ExecutionOutcome { result: ToolResult::err(format!("Outil inconnu : {tool_name}")), terminal_id: None },
        }
    }

    async fn execute_shell_via_acp_terminal(
        &self,
        arguments: &serde_json::Value,
        call_id: &ToolCallId,
    ) -> anyhow::Result<ExecutionOutcome> {
        let command = arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("paramètre 'command' manquant ou vide"))?;

        ShellSandbox::new().analyze_command(command).map_err(|error| anyhow::anyhow!(error.to_string()))?;

        let timeout = arguments.get("timeout").and_then(|value| value.as_u64()).unwrap_or(30).clamp(1, 120);
        let request = CreateTerminalRequest::new(self.session_id.clone(), "sh")
            .args(vec!["-c".to_owned(), command.to_owned()])
            .cwd(self.cwd.to_path_buf())
            .output_byte_limit(64 * 1024);

        let response = self.cx.send_request(request).block_task().await?;
        let terminal_id = response.terminal_id;

        self.emit_update(
            call_id,
            ToolCallStatus::InProgress,
            vec![ToolCallContent::Terminal(Terminal::new(terminal_id.clone()))],
            vec![],
        );

        let wait = WaitForTerminalExitRequest::new(self.session_id.clone(), terminal_id.clone());
        let wait_response = tokio::time::timeout(
            std::time::Duration::from_secs(timeout + 5),
            self.cx.send_request(wait).block_task(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("terminal timeout après {timeout}s"))??;

        let output_response = self
            .cx
            .send_request(TerminalOutputRequest::new(self.session_id.clone(), terminal_id.clone()))
            .block_task()
            .await?;

        let output = output_response.output;
        let exit_code = wait_response.exit_status.as_ref().and_then(|status| status.exit_code);
        let is_ok = exit_code.unwrap_or(0) == 0;

        let _ = self
            .cx
            .send_request(ReleaseTerminalRequest::new(self.session_id.clone(), terminal_id.clone()))
            .block_task()
            .await;

        let text = if output.trim().is_empty() {
            match exit_code { Some(code) => format!("exit code {code}"), None => "(sortie vide)".to_owned() }
        } else if output_response.truncated {
            format!("{output}\n… (sortie tronquée par le client ACP)")
        } else {
            output
        };

        Ok(ExecutionOutcome { result: ToolResult { content: text, is_ok }, terminal_id: Some(terminal_id.0.to_string()) })
    }

    pub async fn request_permission(&self, request: &PermissionRequest, call_id: &ToolCallId) -> PermissionResult {
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

    fn emit_tool_call(&self, call_id: &ToolCallId, info: &ToolInfo, status: ToolCallStatus, raw_input: &serde_json::Value) {
        let tool = AcpToolCall::new(call_id.clone(), info.title.clone())
            .kind(info.kind)
            .status(status)
            .content(info.content.clone())
            .locations(info.locations.clone())
            .raw_input(bounded_raw_input(raw_input));
        let _ = self.cx.send_notification(SessionNotification::new(self.session_id.clone(), SessionUpdate::ToolCall(tool)));
    }

    fn emit_status(&self, call_id: &ToolCallId, status: ToolCallStatus) {
        self.emit_update(call_id, status, vec![], vec![]);
    }

    fn emit_update(&self, call_id: &ToolCallId, status: ToolCallStatus, content: Vec<ToolCallContent>, locations: Vec<agent_client_protocol::schema::v1::ToolCallLocation>) {
        let update = ToolCallUpdate::new(
            call_id.clone(),
            ToolCallUpdateFields::new().status(status).content(content).locations(locations),
        );
        let _ = self.cx.send_notification(SessionNotification::new(self.session_id.clone(), SessionUpdate::ToolCallUpdate(update)));
    }

    fn emit_failed(&self, call_id: &ToolCallId, message: &str, args: &serde_json::Value, tool_name: &str) {
        let rendered = result_update(tool_name, args, message, false, self.cwd, None);
        self.emit_update(call_id, rendered.status, rendered.content, rendered.locations);
    }
}

fn registry_result(result: RegistryToolResult) -> ToolResult {
    match result {
        RegistryToolResult::Ok(content) => ToolResult { content, is_ok: true },
        RegistryToolResult::Err(content) => ToolResult { content, is_ok: false },
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
    fn permission_kind_mapping() {
        assert_eq!(PermissionKind::Write.label(), "write");
        assert_eq!(PermissionKind::Execute.label(), "execute");
    }

    #[test]
    fn stop_reason_mapping() {
        use agent_client_protocol::schema::v1::StopReason;
        assert_eq!(map_stop_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(map_stop_reason(Some("content_filter")), StopReason::Refusal);
        assert_eq!(map_stop_reason(None), StopReason::EndTurn);
    }
}
