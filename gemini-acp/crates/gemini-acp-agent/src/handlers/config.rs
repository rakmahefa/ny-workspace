//! Handler `session/set_config_option` (refactor M9 §6.1 — extrait de `main.rs`).
//!
//! Émet `ConfigOptionUpdate` après mise à jour (refactor M7 §3.6).
//!
//! **BUG-03** : utilise `store.update_session` (persistance seule) au lieu
//! de `store.end_turn` (persistance + libération du verrou busy), pour
//! éviter de créer une course condition avec un tour actif.

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError, Responder};

use gemini_acp_runtime::AppState;
use gemini_acp_config::config::config_options::build_config_options;
use gemini_acp_runtime::state::Session;

pub async fn handle(
    req: SetSessionConfigOptionRequest,
    responder: Responder<SetSessionConfigOptionResponse>,
    state: &AppState,
    cx: &ConnectionTo<Client>,
) -> Result<(), AcpError> {
    // Vérifier que la session existe.
    if state.store.get(&req.session_id.0).await.is_none() {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "session_id": req.session_id.0.to_string() })),
        );
    }

    // Déterminer les modifications à appliquer.
    let config_id = req.config_id.0.clone();
    let value = req.value.clone();

    // Appliquer via update_session (persistance SANS toucher busy).
    let session = match state
        .store
        .update_session(&req.session_id.0, move |s: &mut Session| {
            match config_id.as_ref() {
                "model" => {
                    if let Some(v) = value.as_value_id() {
                        s.model = v.to_string();
                    }
                }
                "think" => {
                    if let Some(v) = value.as_value_id() {
                        if let Ok(n) = v.0.parse::<u32>() {
                            s.think = Some(n.min(4));
                        }
                    }
                }
                "tools_enabled" => {
                    if let Some(v) = value.as_value_id() {
                        match v.0.as_ref().to_ascii_lowercase().as_str() {
                            "true" | "1" | "on" | "yes" => s.tools_enabled = true,
                            "false" | "0" | "off" | "no" => s.tools_enabled = false,
                            other => {
                                // On ne peut pas retourner d'erreur depuis la closure,
                                // mais la validation ci-dessous attrapera le cas.
                                // On laisse la valeur inchangée.
                                tracing::warn!(
                                    value = other,
                                    "valeur tools_enabled invalide, ignorée"
                                );
                            }
                        }
                    }
                }
                other => {
                    tracing::warn!(config_id = other, "config_id inconnu");
                }
            }
        })
        .await
    {
        Ok(()) => state.store.get(&req.session_id.0).await,
        Err(e) => {
            return responder.respond_with_internal_error(format!("{e:#}"));
        }
    };

    let Some(session) = session else {
        return responder.respond_with_error(
            AcpError::invalid_params()
                .data(serde_json::json!({ "session_id": req.session_id.0.to_string() })),
        );
    };

    let options = build_config_options(&session.model, session.think, session.tools_enabled);

    // Émettre ConfigOptionUpdate (best-effort) — refactor M7 §3.6.
    cx.send_notification(SessionNotification::new(
        req.session_id.clone(),
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options.clone())),
    ))?;

    responder.respond(SetSessionConfigOptionResponse::new(options))
}
