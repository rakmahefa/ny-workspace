//! Handlers de cycle de vie des sessions.
//!
//! Refactor R1 — inspiré de `glm-acp-agent/src/protocol/agent.ts` :
//! - **handle_set_mode** : nouveau handler `session/set_mode` (inspiré de
//!   `GlmAcpAgent.setSessionMode`). Émet `CurrentModeUpdate` après changement.
//! - **handle_fork** : nouveau handler `session/fork` (inspiré de
//!   `GlmAcpAgent.unstable_forkSession`).
//! - **Modes state** : les modes sont retournés dans les réponses session/new,
//!   load, resume, fork.

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError, Responder};
use tracing::warn;

use gemini_acp_runtime::AppState;
use gemini_acp_config::config::config_options::build_config_options;
use gemini_acp_runtime::state::{Role, SessionMode as AcpSessionMode};

/// Valide qu'un identifiant de session correspond au format attendu
/// `^sess_[a-f0-9]{32}$` (UUID v4 simplifié, hexadécimal minuscule).
fn is_valid_session_id(id: &str) -> bool {
    let rest = match id.strip_prefix("sess_") {
        Some(r) => r,
        None => return false,
    };
    rest.len() == 32
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Construit la liste des modes disponibles (schema ACP v1.5.0).
/// Inspiré de `GlmAcpAgent.modesState()`.
fn build_available_modes() -> Vec<SessionMode> {
    AcpSessionMode::all()
        .iter()
        .map(|m| {
            SessionMode::new(
                SessionModeId::from(match m {
                    AcpSessionMode::Default => "default",
                    AcpSessionMode::AcceptEdits => "accept_edits",
                    AcpSessionMode::BypassPermissions => "bypass_permissions",
                }),
                m.display_name(),
            )
            .description(m.description())
        })
        .collect()
}

/// Construit l'état des modes (mode courant + modes disponibles).
fn build_mode_state(current: AcpSessionMode) -> SessionModeState {
    let current_id = SessionModeId::from(match current {
        AcpSessionMode::Default => "default",
        AcpSessionMode::AcceptEdits => "accept_edits",
        AcpSessionMode::BypassPermissions => "bypass_permissions",
    });
    SessionModeState::new(current_id, build_available_modes())
}

/// `session/new` — crée une session persistée.
pub async fn handle_new(
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !req.cwd.is_absolute() {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "cwd": "le chemin doit être absolu" })),
        );
    }
    match tokio::fs::metadata(&req.cwd).await {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return responder.respond_with_error(AcpError::invalid_params().data(
                serde_json::json!({
                    "cwd": req.cwd.to_string_lossy(),
                    "error": "le chemin n'est pas un répertoire"
                }),
            ));
        }
        Err(e) => {
            return responder.respond_with_error(AcpError::invalid_params().data(
                serde_json::json!({
                    "cwd": req.cwd.to_string_lossy(),
                    "error": format!("chemin inaccessible: {e}")
                }),
            ));
        }
    }
    if !req.mcp_servers.is_empty() {
        warn!(
            n = req.mcp_servers.len(),
            "session/new ignore les mcp_servers (pas de MCP en v1)"
        );
    }
    match state
        .store
        .create(
            req.cwd.clone(),
            req.additional_directories.clone(),
            &state.config.default_model,
        )
        .await
    {
        Ok(session) => responder.respond(
            NewSessionResponse::new(session.id.clone())
                .config_options(build_config_options(&session.model, session.think, session.tools_enabled))
                .modes(build_mode_state(AcpSessionMode::Default)),
        ),
        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
    }
}

/// `session/list` — liste le dépôt (filtre `cwd` si fourni).
pub async fn handle_list(
    req: ListSessionsRequest,
    responder: Responder<ListSessionsResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    let sessions = state.store.list(req.cwd.as_deref()).await;
    let infos = sessions
        .iter()
        .map(|s| {
            SessionInfo::new(SessionId::from(s.id.clone()), s.cwd.clone())
                .additional_directories(s.additional_directories.clone())
                .title(s.title.clone())
                .updated_at(Some(s.updated_at.clone()))
        })
        .collect();
    responder.respond(ListSessionsResponse::new(infos))?;
    Ok(())
}

/// `session/load` — rejeu de l'historique AVANT la réponse (spec §3.2).
///
/// Refactor R1 : retourne aussi les modes (inspiré de GlmAcpAgent.loadSession).
pub async fn handle_load(
    req: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    state: &AppState,
    cx: &ConnectionTo<Client>,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(AcpError::invalid_params().data(serde_json::json!({
            "session_id": req.session_id.0.to_string(),
            "error": "identifiant de session invalide"
        })));
    }
    let Some(session) = state.store.get(&req.session_id.0).await else {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "session_id": req.session_id.0.to_string() })),
        );
    };
    if req.cwd != session.cwd {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "cwd": "ne correspond pas à la session" })),
        );
    }
    // Rejeu de l'historique (inspiré de GlmAcpAgent.replayMessages).
    for (i, (role, text)) in session.messages.iter().enumerate() {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
            .message_id(MessageId::from(format!("msg_{i}")));
        let update = match role {
            Role::User => SessionUpdate::UserMessageChunk(chunk),
            Role::Assistant => SessionUpdate::AgentMessageChunk(chunk),
            Role::Tool => continue,
        };
        cx.send_notification(SessionNotification::new(req.session_id.clone(), update))?;
    }
    // Retourner les modes et config options (inspiré de GlmAcpAgent.loadSession).
    responder.respond(
        LoadSessionResponse::new()
            .config_options(build_config_options(&session.model, session.think, session.tools_enabled))
            .modes(build_mode_state(session.mode)),
    )
}

/// `session/resume` — restauration sans rejeu.
///
/// Refactor R1 : retourne aussi les modes.
pub async fn handle_resume(
    req: ResumeSessionRequest,
    responder: Responder<ResumeSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(AcpError::invalid_params().data(serde_json::json!({
            "session_id": req.session_id.0.to_string(),
            "error": "identifiant de session invalide"
        })));
    }
    let Some(session) = state.store.get(&req.session_id.0).await else {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "session_id": req.session_id.0.to_string() })),
        );
    };
    if req.cwd != session.cwd {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "cwd": "ne correspond pas à la session" })),
        );
    }
    responder.respond(
        ResumeSessionResponse::new()
            .config_options(build_config_options(&session.model, session.think, session.tools_enabled))
            .modes(build_mode_state(session.mode)),
    )
}

/// `session/delete` — supprime la session (mémoire + fichier).
pub async fn handle_delete(
    req: DeleteSessionRequest,
    responder: Responder<DeleteSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(AcpError::invalid_params().data(serde_json::json!({
            "session_id": req.session_id.0.to_string(),
            "error": "identifiant de session invalide"
        })));
    }
    state.store.delete(&req.session_id.0).await;
    responder.respond(DeleteSessionResponse::new())
}

/// `session/close` — annule + libère, fichier conservé.
pub async fn handle_close(
    req: CloseSessionRequest,
    responder: Responder<CloseSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(AcpError::invalid_params().data(serde_json::json!({
            "session_id": req.session_id.0.to_string(),
            "error": "identifiant de session invalide"
        })));
    }
    state.store.close(&req.session_id.0).await;
    responder.respond(CloseSessionResponse::new())
}

/// `session/set_mode` — change le mode de permission.
///
/// Inspiré de `GlmAcpAgent.setSessionMode()` :
/// - Valide le mode.
/// - Met à jour la session.
/// - Persiste.
/// - Émet `CurrentModeUpdate`.
/// - Émet `ConfigOptionUpdate` si nécessaire.
pub async fn handle_set_mode(
    req: SetSessionModeRequest,
    responder: Responder<SetSessionModeResponse>,
    state: &AppState,
    cx: &ConnectionTo<Client>,
) -> Result<(), AcpError> {
    let new_mode = match AcpSessionMode::from_str_lossy(&req.mode_id.0) {
        Some(m) => m,
        None => {
            let valid = AcpSessionMode::all()
                .iter()
                .map(|m| match m {
                    AcpSessionMode::Default => "default",
                    AcpSessionMode::AcceptEdits => "accept_edits",
                    AcpSessionMode::BypassPermissions => "bypass_permissions",
                })
                .collect::<Vec<_>>()
                .join(", ");
            return responder.respond_with_error(AcpError::invalid_params().data(
                serde_json::json!({
                    "mode_id": req.mode_id.0.to_string(),
                    "error": format!("mode_id invalide. Modes valides: {valid}")
                }),
            ));
        }
    };

    // Vérifier que la session existe.
    if state.store.get(&req.session_id.0).await.is_none() {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "session_id": req.session_id.0.to_string() })),
        );
    }

    // Mettre à jour le mode.
    if let Err(e) = state
        .store
        .update_session(&req.session_id.0, move |s| {
            s.mode = new_mode;
            s.updated_at = gemini_acp_config::core::time::now_iso();
        })
        .await
    {
        return responder.respond_with_internal_error(format!("{e:#}"));
    }

    // Émettre CurrentModeUpdate (inspiré de GlmAcpAgent.setSessionMode).
    cx.send_notification(SessionNotification::new(
        req.session_id.clone(),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(
            match new_mode {
                AcpSessionMode::Default => "default",
                AcpSessionMode::AcceptEdits => "accept_edits",
                AcpSessionMode::BypassPermissions => "bypass_permissions",
            },
        ))),
    ))?;

    responder.respond(SetSessionModeResponse::new())
}

/// `session/fork` — duplique une session existante (inspiré de GlmAcpAgent.unstable_forkSession).
///
/// Crée une nouvelle session avec le même historique, cwd, et configuration.
/// Le titre est suffixé de " (fork)". Les tours sont réinitialisés.
pub async fn handle_fork(
    req: ForkSessionRequest,
    responder: Responder<ForkSessionResponse>,
    state: &AppState,
) -> Result<(), AcpError> {
    if !is_valid_session_id(&req.session_id.0) {
        return responder.respond_with_error(AcpError::invalid_params().data(serde_json::json!({
            "session_id": req.session_id.0.to_string(),
            "error": "identifiant de session invalide"
        })));
    }

    // Vérifier que la session source existe.
    if state.store.get(&req.session_id.0).await.is_none() {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "session_id": req.session_id.0.to_string() })),
        );
    }

    // Exécuter le fork via le store.
    match state.store.fork(&req.session_id.0).await {
        Ok(forked) => responder.respond(
            ForkSessionResponse::new(SessionId::from(forked.id.clone()))
                .config_options(build_config_options(&forked.model, forked.think, forked.tools_enabled))
                .modes(build_mode_state(forked.mode)),
        ),
        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_valide_format() {
        assert!(is_valid_session_id("sess_0123456789abcdef0123456789abcdef"));
        assert!(is_valid_session_id("sess_aabbccddeeff00112233445566778899"));
    }

    #[test]
    fn session_id_rejette_path_traversal() {
        assert!(!is_valid_session_id("../../etc/passwd"));
        assert!(!is_valid_session_id("sess_../../etc"));
        assert!(!is_valid_session_id("sess_/etc/passwd"));
        assert!(!is_valid_session_id(
            "../sess_0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_valid_session_id(
            "sess_0123456789abcdef0123456789abcde/g"
        ));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("sess_"));
        assert!(!is_valid_session_id("sess_short"));
        assert!(!is_valid_session_id(
            "sess_0123456789ABCDEF0123456789ABCDEF"
        ));
        assert!(!is_valid_session_id(
            "other_0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn build_modes_retourne_3_modes() {
        let modes = build_available_modes();
        assert_eq!(modes.len(), 3);
        let ids: Vec<&str> = modes.iter().map(|m| m.id.0.as_ref()).collect();
        assert!(ids.contains(&"default"));
        assert!(ids.contains(&"accept_edits"));
        assert!(ids.contains(&"bypass_permissions"));
    }
}
