//! Notifications ACP : chunks texte, usage tokens (spec §3.3 + §5.2).
//!
//! Responsabilités :
//! - `notify_text` : émet un `AgentMessageChunk` vers le client.
//! - `notify_usage` : émet un `UsageUpdate` (tokens estimés) en fin de tour.
//! - `usage_update` : estimation grossière des tokens (chars / 4).

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionId, SessionNotification, SessionUpdate,
    TextContent, UsageUpdate,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

/// Fenêtre de contexte (tokens) des modèles Gemini web — borne haute du `UsageUpdate`.
pub const CONTEXT_TOKENS: u64 = 1_000_000;

/// Estimation grossière des tokens en contexte (chars / 4, même formule que
/// l'API, spec §5.2) pour le `UsageUpdate` ACP envoyé en fin de tour.
pub fn usage_update(prompt: &str, assistant: &str) -> UsageUpdate {
    let used = (prompt.chars().count() + assistant.chars().count()) as u64 / 4;
    UsageUpdate::new(used, CONTEXT_TOKENS)
}

/// Émet un `AgentMessageChunk` pour le texte donné.
pub fn notify_text(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message_id: &MessageId,
    text: String,
) -> Result<(), AcpError> {
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                .message_id(message_id.clone()),
        ),
    ))
}

/// Notification `UsageUpdate` (tokens estimés en contexte) en fin de tour.
pub fn notify_usage(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    prompt: &str,
    assistant: &str,
) -> Result<(), AcpError> {
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::UsageUpdate(usage_update(prompt, assistant)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_estime_tokens_en_contexte() {
        // Symboles multi-octets comptés en chars ; taille de fenêtre constante.
        let u = usage_update("question 🚀", "réponse");
        assert_eq!(u.used, (10 + 8) / 4);
        assert_eq!(u.size, CONTEXT_TOKENS);
        assert!(u.cost.is_none());
    }
}
