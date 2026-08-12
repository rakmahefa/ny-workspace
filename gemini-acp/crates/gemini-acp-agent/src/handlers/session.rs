//! ACP session lifecycle handlers.
//!
//! The handler layer deliberately delegates lifecycle invariants to
//! `gemini_acp_runtime::SessionManager`. This keeps validation, persistence and
//! user-visible error semantics consistent across new/load/resume/fork/close.
//!
//! UX principles borrowed from the mature Claude ACP adapter:
//! - session mode state is returned on every lifecycle response;
//! - loading replays history before resolving the request;
//! - the session title is restored as an explicit `session/update`, so clients
//!   can paint the conversation header before the first history chunk arrives;
//! - mode changes emit `CurrentModeUpdate` immediately instead of requiring a
//!   client refresh;
//! - invalid lifecycle inputs are rejected as `invalid_params` rather than
//!   leaking storage errors to the UI;
//! - persisted tool_call/tool_result blocks are reconstructed into real ACP
//!   tool cards during replay instead of disappearing from the conversation.

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError, Responder};
use tracing::warn;

use gemini_acp_config::config::config_options::build_config_options;
use gemini_acp_runtime::state::{Role, SessionMode as AcpSessionMode};
use gemini_acp_runtime::tools::parse::parse_tool_calls;
use gemini_acp_runtime::tools::tool_ux::{result_update, ToolInfo};
use gemini_acp_runtime::AppState;

fn is_valid_session_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("sess_") else {
        return false;
    };
    rest.len() == 32
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn session_id_error(id: &SessionId) -> AcpError {
    AcpError::invalid_params().data(serde_json::json!({
        "session_id": id.to_string(),
        "error": "identifiant de session invalide"
    }))
}

fn session_mode_id(mode: AcpSessionMode) -> SessionModeId {
    SessionModeId::from(match mode {
        AcpSessionMode::Default => "default",
        AcpSessionMode::AcceptEdits => "accept_edits",
        AcpSessionMode::BypassPermissions => "bypass_permissions",
    })
}

fn build_available_modes() -> Vec<SessionMode> {
    AcpSessionMode::all()
        .iter()
        .map(|mode| {
            SessionMode::new(session_mode_id(*mode), mode.display_name())
                .description(mode.description())
        })
        .collect()
}

fn build_mode_state(current: AcpSessionMode) -> SessionModeState {
    SessionModeState::new(session_mode_id(current), build_available_modes())
}

fn send_restored_title(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    title: Option<&str>,
) -> Result<(), AcpError> {
    let Some(title) = title else {
        return Ok(());
    };

    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title.to_owned())),
    ))?;
    Ok(())
}

fn replay_tool_result(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    tool_call_index: usize,
    tool_name: &str,
    args: &serde_json::Value,
    result_text: Option<&str>,
    cwd: &std::path::Path,
) -> Result<(), AcpError> {
    let call_id = ToolCallId::from(format!("replay_call_{tool_call_index}"));
    let info = ToolInfo::build(tool_name, args, cwd, None);
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::ToolCall(
            ToolCall::new(call_id.clone(), info.title.clone())
                .kind(info.kind)
                .status(if result_text.is_some() {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::InProgress
                })
                .content(info.content.clone())
                .locations(info.locations.clone())
                .raw_input(gemini_acp_runtime::tools::tool_ux::bounded_raw_input(args)),
        ),
    ))?;

    if let Some(result_text) = result_text {
        let rendered = result_update(tool_name, args, result_text, true, cwd, None);
        cx.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .status(rendered.status)
                    .content(rendered.content)
                    .locations(rendered.locations),
            )),
        ))?;
    }

    Ok(())
}

/// Replay a persisted assistant message and its immediately-following tool results.
///
/// The current store persists textual `tool_call` and `tool_result` blocks. We
/// deliberately use those blocks as the replay source instead of inventing a
/// second persistence schema. This keeps replay faithful to what is already
/// stored while restoring genuine ACP ToolCall / ToolCallUpdate events.
fn replay_assistant_with_tools(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message_id: MessageId,
    text: &str,
    following_tool_results: &mut std::slice::Iter<'_, (Role, String)>,
    cwd: &std::path::Path,
    replay_index: &mut usize,
) -> Result<(), AcpError> {
    let (clean_text, calls) = parse_tool_calls(text);
    if !clean_text.trim().is_empty() {
        cx.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(clean_text)))
                    .message_id(message_id),
            ),
        ))?;
    }

    for call in calls {
        let result = following_tool_results
            .clone()
            .find_map(|(role, text)| {
                if *role == Role::Tool {
                    Some(text.as_str())
                } else {
                    None
                }
            });
        replay_tool_result(
            cx,
            session_id,
            *replay_index,
            &call.name,
            &call.arguments,
            result,
            cwd,
        )?;
        *replay_index += 1;
    }

    Ok(())
}

/// `session/new`: persist a new session and return the current configuration/mode state.
pub async fn handle_new(
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !req.mcp_servers.is_empty() {
        warn!(
            count = req.mcp_servers.len(),
            "session/new received mcp_servers, but Gemini ACP does not wire them yet"
        );
    }

    match state
        .sessions
        .create(
            req.cwd.clone(),
            req.additional_directories.clone(),
            &state.config.default_model,
        )
        .await
    {
        Ok(session) => responder.respond(
            NewSessionResponse::new(session.id.clone())
                .config_options(build_config_options(
                    &session.model,
                    session.think,
                    session.tools_enabled,
                ))
                .modes(build_mode_state(session.mode)),
        ),
        Err(error) => responder.respond_with_internal_error(format!("création de session: {error:#}")),
    }
}

pub async fn handle_list(
    req: ListSessionsRequest,
    responder: Responder<ListSessionsResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    let sessions = match state.sessions.list(req.cwd.as_deref()).await {
        Ok(sessions) => sessions,
        Err(error) => {
            return responder
                .respond_with_internal_error(format!("liste des sessions: {error:#}"));
        }
    };

    let infos = sessions
        .into_iter()
        .map(|session| {
            SessionInfo::new(SessionId::from(session.id), session.cwd)
                .additional_directories(session.additional_directories)
                .title(session.title)
                .updated_at(Some(session.updated_at))
        })
        .collect();

    responder.respond(ListSessionsResponse::new(infos))
}

pub async fn handle_load(
    req: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    state: &AppState,
    cx: &ConnectionTo<Client>,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(session_id_error(&req.session_id));
    }

    let session = match state.sessions.load(&req.session_id.0, &req.cwd).await {
        Ok(session) => session,
        Err(error) => {
            return responder.respond_with_error(
                AcpError::invalid_params().data(serde_json::json!({
                    "session_id": req.session_id.to_string(),
                    "error": format!("session introuvable ou workspace incompatible: {error:#}")
                })),
            );
        }
    };

    send_restored_title(cx, &req.session_id, session.title.as_deref())?;

    let mut replay_index = 0usize;
    let mut index = 0usize;
    while index < session.messages.len() {
        let (role, text) = &session.messages[index];
        match role {
            Role::User => {
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
                    .message_id(MessageId::from(format!("msg_{index}")));
                cx.send_notification(SessionNotification::new(
                    req.session_id.clone(),
                    SessionUpdate::UserMessageChunk(chunk),
                ))?;
            }
            Role::Assistant => {
                let (_, calls) = parse_tool_calls(text);
                let mut result_cursor = index + 1;
                let mut results = Vec::new();
                while result_cursor < session.messages.len()
                    && session.messages[result_cursor].0 == Role::Tool
                    && results.len() < calls.len()
                {
                    results.push(session.messages[result_cursor].1.clone());
                    result_cursor += 1;
                }

                let mut result_iter = results.iter().map(|text| (Role::Tool, text.clone())).collect::<Vec<_>>();
                let mut result_refs = result_iter.iter().map(|entry| (&entry.0, &entry.1));
                let mut tool_results = Vec::new();
                while let Some((role, text)) = result_refs.next() {
                    if *role == Role::Tool {
                        tool_results.push(text.clone());
                    }
                }

                let (clean_text, calls) = parse_tool_calls(text);
                if !clean_text.trim().is_empty() {
                    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(clean_text)))
                        .message_id(MessageId::from(format!("msg_{index}")));
                    cx.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(chunk),
                    ))?;
                }

                for (call_index, call) in calls.iter().enumerate() {
                    let result_text = tool_results.get(call_index).map(String::as_str);
                    replay_tool_result(
                        cx,
                        &req.session_id,
                        replay_index,
                        &call.name,
                        &call.arguments,
                        result_text,
                        &session.cwd,
                    )?;
                    replay_index += 1;
                }

                index = result_cursor.saturating_sub(1);
            }
            Role::Tool => {
                // Tool results are consumed together with the preceding assistant
                // tool_call message so the client receives a coherent card.
            }
        }
        index += 1;
    }

    responder.respond(
        LoadSessionResponse::new()
            .config_options(build_config_options(
                &session.model,
                session.think,
                session.tools_enabled,
            ))
            .modes(build_mode_state(session.mode)),
    )
}

pub async fn handle_resume(
    req: ResumeSessionRequest,
    responder: Responder<ResumeSessionResponse>,
    state: &AppState,
    cx: &ConnectionTo<Client>,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(session_id_error(&req.session_id));
    }

    let session = match state.sessions.resume(&req.session_id.0, &req.cwd).await {
        Ok(session) => session,
        Err(error) => {
            return responder.respond_with_error(
                AcpError::invalid_params().data(serde_json::json!({
                    "session_id": req.session_id.to_string(),
                    "error": format!("session introuvable ou workspace incompatible: {error:#}")
                })),
            );
        }
    };

    send_restored_title(cx, &req.session_id, session.title.as_deref())?;

    responder.respond(
        ResumeSessionResponse::new()
            .config_options(build_config_options(
                &session.model,
                session.think,
                session.tools_enabled,
            ))
            .modes(build_mode_state(session.mode)),
    )
}

pub async fn handle_delete(
    req: DeleteSessionRequest,
    responder: Responder<DeleteSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(session_id_error(&req.session_id));
    }

    match state.sessions.delete(&req.session_id.0).await {
        Ok(true) => responder.respond(DeleteSessionResponse::new()),
        Ok(false) => responder.respond_with_error(
            AcpError::invalid_params().data(serde_json::json!({
                "session_id": req.session_id.to_string(),
                "error": "session introuvable"
            })),
        ),
        Err(error) => responder.respond_with_internal_error(format!("suppression de session: {error:#}")),
    }
}

pub async fn handle_close(
    req: CloseSessionRequest,
    responder: Responder<CloseSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(session_id_error(&req.session_id));
    }

    match state.sessions.close(&req.session_id.0).await {
        Ok(true) => responder.respond(CloseSessionResponse::new()),
        Ok(false) => responder.respond_with_error(
            AcpError::invalid_params().data(serde_json::json!({
                "session_id": req.session_id.to_string(),
                "error": "session introuvable"
            })),
        ),
        Err(error) => responder.respond_with_internal_error(format!("fermeture de session: {error:#}")),
    }
}

pub async fn handle_set_mode(
    req: SetSessionModeRequest,
    responder: Responder<SetSessionModeResponse>,
    state: &AppState,
    cx: &ConnectionTo<Client>,
) -> Result<(), AcpError> {
    let Some(new_mode) = AcpSessionMode::from_str_lossy(&req.mode_id.0) else {
        let valid = AcpSessionMode::all()
            .iter()
            .map(|mode| session_mode_id(*mode).0.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return responder.respond_with_error(
            AcpError::invalid_params().data(serde_json::json!({
                "mode_id": req.mode_id.to_string(),
                "error": format!("mode_id invalide. Modes valides: {valid}")
            })),
        );
    };

    let updated = match state.sessions.set_mode(&req.session_id.0, new_mode).await {
        Ok(session) => session,
        Err(error) => {
            return responder.respond_with_error(
                AcpError::invalid_params().data(serde_json::json!({
                    "session_id": req.session_id.to_string(),
                    "error": format!("impossible de changer le mode: {error:#}")
                })),
            );
        }
    };

    cx.send_notification(SessionNotification::new(
        req.session_id.clone(),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(session_mode_id(updated.mode))),
    ))?;

    responder.respond(SetSessionModeResponse::new())
}

pub async fn handle_fork(
    req: ForkSessionRequest,
    responder: Responder<ForkSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(session_id_error(&req.session_id));
    }

    match state.sessions.fork(&req.session_id.0).await {
        Ok(forked) => responder.respond(
            ForkSessionResponse::new(SessionId::from(forked.id.clone()))
                .config_options(build_config_options(
                    &forked.model,
                    forked.think,
                    forked.tools_enabled,
                ))
                .modes(build_mode_state(forked.mode)),
        ),
        Err(error) => responder.respond_with_error(
            AcpError::invalid_params().data(serde_json::json!({
                "session_id": req.session_id.to_string(),
                "error": format!("fork impossible: {error:#}")
            })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_accepts_only_expected_format() {
        assert!(is_valid_session_id("sess_0123456789abcdef0123456789abcdef"));
        assert!(is_valid_session_id("sess_aabbccddeeff00112233445566778899"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("sess_short"));
        assert!(!is_valid_session_id("sess_0123456789abcdef0123456789ABCDEF"));
        assert!(!is_valid_session_id("../sess_0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_session_id("sess_/etc/passwd"));
    }

    #[test]
    fn all_modes_have_stable_acp_ids() {
        let modes = build_available_modes();
        let ids: Vec<&str> = modes.iter().map(|mode| mode.id.0.as_ref()).collect();
        assert_eq!(ids, vec!["default", "accept_edits", "bypass_permissions"]);
    }

    #[test]
    fn mode_state_uses_current_mode() {
        let state = build_mode_state(AcpSessionMode::AcceptEdits);
        assert_eq!(state.current_mode_id.0, "accept_edits");
        assert_eq!(state.available_modes.len(), 3);
    }
}
