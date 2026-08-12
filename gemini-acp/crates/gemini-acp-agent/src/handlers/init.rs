//! Handler `initialize`.

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Error as AcpError, Responder};

use gemini_acp_runtime::AppState;
use gemini_acp_config::config::config_options::build_agent_capabilities;

#[cfg(feature = "elicitation")]
use crate::elicitation::support_from_client_capabilities;

pub async fn handle(req: InitializeRequest, responder: Responder<InitializeResponse>, state: &AppState) -> Result<(), AcpError> {
    #[cfg(feature = "elicitation")]
    {
        let support = support_from_client_capabilities(req.client_capabilities.elicitation.as_ref());
        *state.elicitation.write().await = support;
        gemini_acp_runtime::tools::interactive::set_elicitation_support(support).await;
        tracing::info!(form = support.form, url = support.url, "capacités d'elicitation négociées");
    }

    let mut caps = build_agent_capabilities();
    caps.session_capabilities = caps.session_capabilities.fork(SessionForkCapabilities::new());

    responder.respond(
        InitializeResponse::new(req.protocol_version)
            .agent_capabilities(caps)
            .auth_methods(vec![])
            .agent_info(Implementation::new("gemini-acp", env!("CARGO_PKG_VERSION")).title("Gemini (Web)")),
    )?;
    Ok(())
}
