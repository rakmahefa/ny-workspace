//! Handler `initialize`.
//!
//! Refactor R1 — inspiré de `GlmAcpAgent.initialize()` :
//! - Annonce la capability `fork` (SessionCapabilities).
//! - Auth methods vides (cookies gérés en interne, comme glm-acp-agent).

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Error as AcpError, Responder};

use gemini_acp_runtime::AppState;
use gemini_acp_config::config::config_options::build_agent_capabilities;

pub async fn handle(
    req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    _state: &AppState,
) -> Result<(), AcpError> {
    let mut caps = build_agent_capabilities();
    // R1: annoncer fork (inspiré de GlmAcpAgent qui a fork: {} dans capabilities).
    caps.session_capabilities = caps
        .session_capabilities
        .fork(SessionForkCapabilities::new());

    responder.respond(
        InitializeResponse::new(req.protocol_version)
            .agent_capabilities(caps)
            .auth_methods(vec![])
            .agent_info(
                Implementation::new("gemini-acp", env!("CARGO_PKG_VERSION")).title("Gemini (Web)"),
            ),
    )?;
    Ok(())
}
