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
//!   leaking storage errors to the UI.

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError, Responder};
use tracing::warn;

use gemini_acp_config::config::config_options::build_config_options;
use gemini_acp_runtime::state::{Role, SessionMode as AcpSessionMode};
use gemini_acp_runtime::AppState;

/// ACP session id contract used by the persistent store: `sess_` + 32 lowercase hex characters.
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

/// `session/list`: list persisted sessions, optionally scoped by cwd.
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

/// `session/load`: restore the full history before resolving the request.
///
/// The title is explicitly replayed as a session update first. This avoids a
/// visually blank/stale conversation header while the message history is
/// streaming to a client.
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

    send_restored_title(&cx, &req.session_id, session.title.as_deref())?;

    for (index, (role, text)) in session.messages.iter().enumerate() {
        let message_id = MessageId::from(format!("msg_{index}"));
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
            .message_id(message_id);

        let update = match role {
            Role::User => SessionUpdate::UserMessageChunk(chunk),
            Role::Assistant => SessionUpdate::AgentMessageChunk(chunk),
            // Tool events are not yet persisted with enough structured fields
            // to reconstruct a genuine ToolCall/ToolCallUpdate safely. Do not
            // fabricate a tool card during history replay.
            Role::Tool => continue,
        };

        cx.send_notification(SessionNotification::new(
            req.session_id.clone(),
            update,
        ))?;
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

/// `session/resume`: validate and restore session state without replaying history.
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

/// `session/delete`: permanently remove a session.
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

/// `session/close`: cancel active work and release the live session, keeping persisted state.
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

/// `session/set_mode`: update the persisted permission mode and immediately notify the client.
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

/// `session/fork`: create a new persisted session from an existing conversation.
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
