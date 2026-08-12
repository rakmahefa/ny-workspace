//! Handler notification `session/cancel`.

use agent_client_protocol::schema::v1::CancelNotification;
use agent_client_protocol::Error as AcpError;

use gemini_acp_runtime::AppState;

pub async fn handle(notif: CancelNotification, state: &AppState) -> Result<(), AcpError> {
    tracing::info!(session = %notif.session_id, "session/cancel");
    state
        .sessions
        .cancel(&notif.session_id.0)
        .await
        .map_err(|e| AcpError::invalid_params().data(serde_json::json!({
            "session_id": notif.session_id.0,
            "error": e.to_string(),
        })))?;
    Ok(())
}
