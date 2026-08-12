//! Deterministic tool executor with Claude-style ACP UX and real ACP permissions.
//!
//! Internal lifecycle: `pending -> permission -> executing -> completed|failed|cancelled`.
//! ACP v1 has no cancelled tool status, so Cancelled is projected to Failed on
//! the wire and explained in `_meta`.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    ContentBlock, CreateTerminalRequest, PermissionOption, PermissionOptionKind,
    ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    SessionNotification, SessionUpdate, Terminal, TerminalOutputRequest, TextContent,
    ToolCall as AcpToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, WaitForTerminalExitRequest,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde_json::{json, Map, Value};

use crate::state::SessionMode;

use super::lifecycle::{
    session_cancelled, wait_for_session_cancel, ToolLifecycle, ToolLifecycleState,
};
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
    pub fn from_tool_call(tool_name: &str, args: &Value, cwd: &Path) -> Self {
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
                if let Some(path) = args.get("path").and_then(Value::as_str) {
                    let resolved = if Path::new(path).is_absolute() { PathBuf::from(path) } else { cwd.join(path) };
                    if resolved.exists() { warnings.push(format!("Le fichier '{}' existe déjà et sera modifié.", info.title)); }
                }
            }
            "shell_exec" => {
                let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                let analysis = ShellAnalysis::analyze(command);
                if analysis.has_dangerous_pipe_chain { warnings.push("Chaîne de commandes potentiellement dangereuse détectée.".into()); }
                if analysis.has_env_injection { warnings.push("Injection de variables d'environnement détectée.".into()); }
                if analysis.risk >= RiskLevel::High { warnings.push(format!("Niveau de risque {} : {}", analysis.risk.emoji(), analysis.risk.description())); }
            }
            _ => {}
        }
        if risk >= RiskLevel::High { warnings.push("Cette opération peut avoir des effets irréversibles.".into()); }

        let detail = if warnings.is_empty() {
            format!("{}\n{} {}", info.title, risk.emoji(), risk.label())
        } else {
            format!("{}\n{} {}\n\nAvertissements :\n{}", info.title, risk.emoji(), risk.label(), warnings.iter().map(|w| format!("  - {w}")).collect::<Vec<_>>().join("\n"))
        };
        Self { kind, risk, summary: info.title, detail, tool_name: tool_name.to_owned(), warnings }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult { pub content: String, pub is_ok: bool }
impl ToolResult { pub fn err(content: impl Into<String>) -> Self { Self { content: content.into(), is_ok: false } } }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult { Allow, Reject, Cancelled, TransportError(String) }

#[derive(Debug)]
struct ExecutionOutcome {
    result: ToolResult,
    terminal_id: Option<String>,
    terminal_meta: Option<Map<String, Value>>,
    cancelled: bool,
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
    pub fn new(cx: &'a ConnectionTo<Client>, session_id: &'a SessionId, registry: &'a ToolRegistry, cwd: &'a Path, additional_dirs: &'a [PathBuf], get_mode: &'a (dyn Fn() -> SessionMode + Send + Sync)) -> Self {
        Self { cx, session_id, registry, cwd, additional_dirs, get_mode }
    }

    pub async fn execute(&self, tool_name: &str, arguments: &Value) -> ToolResult {
        let call_id = ToolCallId::from(format!("call_{}", uuid::Uuid::new_v4().simple()));
        let info = ToolInfo::build(tool_name, arguments, self.cwd, None);
        let mut lifecycle = ToolLifecycle::new();
        self.emit_tool_call(&call_id, &info, &lifecycle, arguments);

        if session_cancelled(self.session_id.0.as_ref()) {
            lifecycle.cancel().expect("pending -> cancelled must be legal");
            let message = "outil annulé avant son démarrage";
            let meta = lifecycle_meta(tool_name, &lifecycle, Some("cancelled"), None);
            self.emit_failed(&call_id, message, arguments, tool_name, Some(meta));
            return ToolResult::err(message);
        }

        let mode = (self.get_mode)();
        let needs_permission = match info.kind {
            ToolKind::Edit | ToolKind::Execute => match mode {
                SessionMode::BypassPermissions => false,
                SessionMode::AcceptEdits => info.kind == ToolKind::Execute && classify_risk(tool_name, arguments) >= RiskLevel::High,
                SessionMode::Default => true,
            },
            _ => false,
        };

        if needs_permission {
            lifecycle.transition(ToolLifecycleState::Permission).expect("pending -> permission must be legal");
            self.emit_lifecycle(&call_id, &lifecycle, tool_name);
            let request = PermissionRequest::from_tool_call(tool_name, arguments, self.cwd);
            match self.request_permission(&request, &call_id).await {
                PermissionResult::Allow => {
                    lifecycle.transition(ToolLifecycleState::Executing).expect("permission -> executing must be legal");
                    self.emit_lifecycle(&call_id, &lifecycle, tool_name);
                }
                PermissionResult::Reject => {
                    lifecycle.transition(ToolLifecycleState::Failed).expect("permission -> failed must be legal");
                    let message = format!("{} ({}) refusé par l'utilisateur.", request.kind.label(), request.summary);
                    let meta = lifecycle_meta(tool_name, &lifecycle, Some("user-rejected"), None);
                    self.emit_failed(&call_id, &message, arguments, tool_name, Some(meta));
                    return ToolResult::err(message);
                }
                PermissionResult::Cancelled => {
                    lifecycle.transition(ToolLifecycleState::Cancelled).expect("permission -> cancelled must be legal");
                    let message = format!("{} ({}) annulé pendant la demande d'autorisation.", request.kind.label(), request.summary);
                    let meta = lifecycle_meta(tool_name, &lifecycle, Some("cancelled"), None);
                    self.emit_failed(&call_id, &message, arguments, tool_name, Some(meta));
                    return ToolResult::err(message);
                }
                PermissionResult::TransportError(error) => {
                    lifecycle.transition(ToolLifecycleState::Failed).expect("permission -> failed must be legal");
                    let message = format!("Échec de la demande de permission ACP : {error}");
                    let meta = lifecycle_meta(tool_name, &lifecycle, Some("permission-error"), None);
                    self.emit_failed(&call_id, &message, arguments, tool_name, Some(meta));
                    return ToolResult::err(message);
                }
            }
        } else {
            lifecycle.transition(ToolLifecycleState::Executing).expect("pending -> executing must be legal");
            self.emit_lifecycle(&call_id, &lifecycle, tool_name);
        }

        let outcome = if tool_name == "shell_exec" {
            match self.execute_shell_via_acp_terminal(arguments, &call_id).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::debug!(session=%self.session_id, error=%error, "terminal ACP indisponible avant exécution, fallback shell local");
                    self.execute_registry(tool_name, arguments).await
                }
            }
        } else {
            self.execute_registry(tool_name, arguments).await
        };

        let next_state = if outcome.cancelled { ToolLifecycleState::Cancelled } else if outcome.result.is_ok { ToolLifecycleState::Completed } else { ToolLifecycleState::Failed };
        lifecycle.transition(next_state).expect("executing must finish in a terminal state");
        let rendered = result_update(tool_name, arguments, &outcome.result.content, outcome.result.is_ok && !outcome.cancelled, self.cwd, outcome.terminal_id.as_deref());
        let meta = lifecycle_meta(tool_name, &lifecycle, if outcome.cancelled { Some("cancelled") } else { None }, outcome.terminal_meta);
        self.emit_update(&call_id, lifecycle.state().wire_status(), rendered.content, rendered.locations, Some(meta));
        outcome.result
    }

    async fn execute_registry(&self, tool_name: &str, arguments: &Value) -> ExecutionOutcome {
        let result = tokio::select! {
            value = self.registry.call_async(tool_name, arguments, self.cwd, self.additional_dirs) => value,
            _ = wait_for_session_cancel(self.session_id.0.as_ref()) => {
                return ExecutionOutcome { result: ToolResult::err("outil annulé pendant son exécution"), terminal_id: None, terminal_meta: None, cancelled: true };
            }
        };
        match result {
            Some(result) => ExecutionOutcome { result: registry_result(result), terminal_id: None, terminal_meta: None, cancelled: false },
            None => ExecutionOutcome { result: ToolResult::err(format!("Outil inconnu : {tool_name}")), terminal_id: None, terminal_meta: None, cancelled: false },
        }
    }

    async fn execute_shell_via_acp_terminal(&self, arguments: &Value, call_id: &ToolCallId) -> anyhow::Result<ExecutionOutcome> {
        let command = arguments.get("command").and_then(Value::as_str).filter(|v| !v.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("paramètre 'command' manquant ou vide"))?;
        ShellSandbox::new().analyze_command(command).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let timeout = arguments.get("timeout").and_then(Value::as_u64).unwrap_or(30).clamp(1, 120);
        let request = CreateTerminalRequest::new(self.session_id.clone(), "sh").args(vec!["-c".to_owned(), command.to_owned()]).cwd(self.cwd.to_path_buf()).output_byte_limit(64 * 1024);

        // Only creation errors fall back. Once a terminal exists, never replay the command locally.
        let response = self.cx.send_request(request).block_task().await?;
        let terminal_id = response.terminal_id;
        self.emit_update(call_id, ToolCallStatus::InProgress, vec![ToolCallContent::Terminal(Terminal::new(terminal_id.clone()))], vec![], Some(terminal_lifecycle_meta(&terminal_id.0.to_string(), None, None)));

        let wait = WaitForTerminalExitRequest::new(self.session_id.clone(), terminal_id.clone());
        let wait_result = tokio::select! {
            result = tokio::time::timeout(std::time::Duration::from_secs(timeout + 5), self.cx.send_request(wait).block_task()) => result,
            _ = wait_for_session_cancel(self.session_id.0.as_ref()) => {
                let _ = self.cx.send_request(ReleaseTerminalRequest::new(self.session_id.clone(), terminal_id.clone())).block_task().await;
                return Ok(ExecutionOutcome {
                    result: ToolResult::err("terminal annulé par session/cancel"),
                    terminal_id: Some(terminal_id.0.to_string()),
                    terminal_meta: Some(terminal_lifecycle_meta(&terminal_id.0.to_string(), None, None)),
                    cancelled: true,
                });
            }
        };

        let (exit_code, signal, wait_error) = match wait_result {
            Ok(Ok(response)) => (response.exit_status.exit_code, response.exit_status.signal, None),
            Ok(Err(error)) => (None, None, Some(error.to_string())),
            Err(_) => (None, None, Some(format!("terminal timeout après {timeout}s"))),
        };

        let output_response = self.cx.send_request(TerminalOutputRequest::new(self.session_id.clone(), terminal_id.clone())).block_task().await;
        let (output, truncated) = match output_response {
            Ok(response) => (response.output, response.truncated),
            Err(error) => (format!("terminal output indisponible: {error}"), false),
        };
        let _ = self.cx.send_request(ReleaseTerminalRequest::new(self.session_id.clone(), terminal_id.clone())).block_task().await;

        let terminal_text = match &wait_error {
            Some(error) if output.trim().is_empty() => error.clone(),
            _ if output.trim().is_empty() => match exit_code { Some(code) => format!("exit code {code}"), None => "(sortie vide)".to_owned() },
            _ if truncated => format!("{output}\n… (sortie tronquée par le client ACP)"),
            _ => output,
        };
        let is_ok = wait_error.is_none() && signal.is_none() && exit_code.unwrap_or(0) == 0;
        Ok(ExecutionOutcome {
            result: ToolResult { content: terminal_text.clone(), is_ok },
            terminal_id: Some(terminal_id.0.to_string()),
            terminal_meta: Some(terminal_lifecycle_meta(&terminal_id.0.to_string(), Some(&terminal_text), Some((exit_code, signal.as_deref())))),
            cancelled: false,
        })
    }

    pub async fn request_permission(&self, request: &PermissionRequest, call_id: &ToolCallId) -> PermissionResult {
        let tool_call = AcpToolCall::new(call_id.clone(), request.summary.clone())
            .kind(match request.kind { PermissionKind::Read => ToolKind::Read, PermissionKind::Write => ToolKind::Edit, PermissionKind::Execute => ToolKind::Execute, PermissionKind::Network => ToolKind::Fetch })
            .status(ToolCallStatus::Pending)
            .meta(permission_meta(request));
        let options = vec![
            PermissionOption::new("allow_once", "Autoriser cette fois", PermissionOptionKind::AllowOnce),
            PermissionOption::new("allow_always", "Toujours autoriser", PermissionOptionKind::AllowAlways),
            PermissionOption::new("reject_once", "Refuser", PermissionOptionKind::RejectOnce),
            PermissionOption::new("reject_always", "Toujours refuser", PermissionOptionKind::RejectAlways),
        ];
        let rpc = RequestPermissionRequest::new(self.session_id.clone(), ToolCallUpdate::from(tool_call), options).meta(permission_meta(request));
        tracing::info!(session=%self.session_id, tool=%request.tool_name, kind=?request.kind, risk=%request.risk, summary=%request.summary, detail=%request.detail, warnings=?request.warnings, "envoi session/request_permission");

        let response = tokio::select! {
            response = self.cx.send_request(rpc).block_task() => match response { Ok(response) => response, Err(error) => return PermissionResult::TransportError(error.to_string()) },
            _ = wait_for_session_cancel(self.session_id.0.as_ref()) => return PermissionResult::Cancelled,
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

    fn emit_tool_call(&self, call_id: &ToolCallId, info: &ToolInfo, lifecycle: &ToolLifecycle, raw_input: &Value) {
        let tool = AcpToolCall::new(call_id.clone(), info.title.clone()).kind(info.kind).status(lifecycle.state().wire_status()).content(info.content.clone()).locations(info.locations.clone()).raw_input(bounded_raw_input(raw_input)).meta(lifecycle_meta(&info.title, lifecycle, None, None));
        let _ = self.cx.send_notification(SessionNotification::new(self.session_id.clone(), SessionUpdate::ToolCall(tool)));
    }

    fn emit_lifecycle(&self, call_id: &ToolCallId, lifecycle: &ToolLifecycle, tool_name: &str) {
        self.emit_update(call_id, lifecycle.state().wire_status(), vec![], vec![], Some(lifecycle_meta(tool_name, lifecycle, None, None)));
    }

    fn emit_update(&self, call_id: &ToolCallId, status: ToolCallStatus, content: Vec<ToolCallContent>, locations: Vec<agent_client_protocol::schema::v1::ToolCallLocation>, meta: Option<Map<String, Value>>) {
        let update = ToolCallUpdate::new(call_id.clone(), ToolCallUpdateFields::new().status(status).content(content).locations(locations)).meta(meta);
        let _ = self.cx.send_notification(SessionNotification::new(self.session_id.clone(), SessionUpdate::ToolCallUpdate(update)));
    }

    fn emit_failed(&self, call_id: &ToolCallId, message: &str, args: &Value, tool_name: &str, meta: Option<Map<String, Value>>) {
        let rendered = result_update(tool_name, args, message, false, self.cwd, None);
        self.emit_update(call_id, ToolCallStatus::Failed, rendered.content, rendered.locations, meta);
    }
}

fn registry_result(result: RegistryToolResult) -> ToolResult {
    match result { RegistryToolResult::Ok(content) => ToolResult { content, is_ok: true }, RegistryToolResult::Err(content) => ToolResult { content, is_ok: false } }
}

fn permission_meta(request: &PermissionRequest) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("claudeCode".into(), json!({ "toolName": request.tool_name, "permission": { "kind": request.kind.label(), "risk": request.risk.label(), "warnings": request.warnings } }));
    meta
}

fn lifecycle_meta(tool_name: &str, lifecycle: &ToolLifecycle, non_execution_kind: Option<&str>, terminal_meta: Option<Map<String, Value>>) -> Map<String, Value> {
    let mut meta = terminal_meta.unwrap_or_default();
    meta.insert("geminiAcp".into(), json!({ "lifecycle": { "state": lifecycle_state_label(lifecycle.state()), "sequence": lifecycle.sequence() } }));
    let claude = meta.entry("claudeCode".into()).or_insert_with(|| json!({}));
    if let Some(object) = claude.as_object_mut() {
        if !tool_name.is_empty() { object.insert("toolName".into(), Value::String(tool_name.to_owned())); }
        if let Some(reason) = non_execution_kind { object.insert("nonExecutionKind".into(), Value::String(reason.to_owned())); }
    }
    meta
}

fn lifecycle_state_label(state: ToolLifecycleState) -> &'static str {
    match state { ToolLifecycleState::Pending => "pending", ToolLifecycleState::Permission => "permission", ToolLifecycleState::Executing => "executing", ToolLifecycleState::Completed => "completed", ToolLifecycleState::Failed => "failed", ToolLifecycleState::Cancelled => "cancelled" }
}

fn terminal_lifecycle_meta(terminal_id: &str, output: Option<&str>, exit: Option<(Option<u32>, Option<&str>)>) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("terminal_info".into(), json!({ "terminal_id": terminal_id }));
    if let Some(output) = output {
        let preview: String = output.chars().take(16_384).collect();
        meta.insert("terminal_output".into(), json!({ "terminal_id": terminal_id, "data": preview }));
    }
    if let Some((exit_code, signal)) = exit {
        meta.insert("terminal_exit".into(), json!({ "terminal_id": terminal_id, "exit_code": exit_code.map(i64::from).unwrap_or(-1), "signal": signal }));
    }
    meta
}

impl PermissionKind { pub fn label(&self) -> &'static str { match self { PermissionKind::Read => "read", PermissionKind::Write => "write", PermissionKind::Execute => "execute", PermissionKind::Network => "network" } } }

pub fn safe_session_update(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) { let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update)); }

pub fn emit_error_chunk(cx: &ConnectionTo<Client>, session_id: &SessionId, message_id: &agent_client_protocol::schema::v1::MessageId, error: &str) {
    safe_session_update(cx, session_id, SessionUpdate::AgentMessageChunk(agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(TextContent::new(format!("\n\n[error] {error}")))).message_id(message_id.clone())));
}

#[allow(dead_code)]
pub fn map_stop_reason(gemini_finish: Option<&str>) -> agent_client_protocol::schema::v1::StopReason {
    match gemini_finish { Some("length") | Some("max_tokens") => agent_client_protocol::schema::v1::StopReason::MaxTokens, Some("content_filter") | Some("safety") | Some("block_reason") => agent_client_protocol::schema::v1::StopReason::Refusal, _ => agent_client_protocol::schema::v1::StopReason::EndTurn }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn permission_kind_mapping() { assert_eq!(PermissionKind::Write.label(), "write"); assert_eq!(PermissionKind::Execute.label(), "execute"); }
    #[test] fn stop_reason_mapping() { use agent_client_protocol::schema::v1::StopReason; assert_eq!(map_stop_reason(Some("length")), StopReason::MaxTokens); assert_eq!(map_stop_reason(Some("content_filter")), StopReason::Refusal); assert_eq!(map_stop_reason(None), StopReason::EndTurn); }
    #[test] fn terminal_metadata_shape() { let meta = terminal_lifecycle_meta("term-1", Some("hello"), Some((Some(0), None))); assert_eq!(meta["terminal_info"]["terminal_id"], "term-1"); assert_eq!(meta["terminal_output"]["data"], "hello"); assert_eq!(meta["terminal_exit"]["exit_code"], 0); assert!(meta["terminal_exit"]["signal"].is_null()); }
}
