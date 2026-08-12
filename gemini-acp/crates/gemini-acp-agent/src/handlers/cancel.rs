//! Handler notification `session/cancel` (refactor M9 §6.1).

use agent_client_protocol::schema::v1::CancelNotification;
use agent_client_protocol::Error as AcpError;

use gemini_acp_runtime::AppState;

pub async fn handle(notif: CancelNotification, state: &AppState) -> Result<(), AcpError> {
    tracing::info!(session = %notif.session_id, "session/cancel");
    state.store.cancel(&notif.session_id.0).await;
    Ok(())
}
