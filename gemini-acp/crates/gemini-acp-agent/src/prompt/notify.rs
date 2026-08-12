//! Notifications ACP : chunks texte, usage tokens.
use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, MessageId, SessionId, SessionNotification, SessionUpdate, TextContent, UsageUpdate};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

pub const CONTEXT_TOKENS: u64 = 1_000_000;

pub fn usage_update(prompt: &str, assistant: &str) -> UsageUpdate {
    let used = (prompt.chars().count() + assistant.chars().count()) as u64 / 4;
    UsageUpdate::new(used, CONTEXT_TOKENS)
}

pub fn notify_text(cx: &ConnectionTo<Client>, session_id: &SessionId, message_id: &MessageId, text: String) -> Result<(), AcpError> {
    cx.send_notification(SessionNotification::new(session_id.clone(), SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(text))).message_id(message_id.clone()))))
}

pub fn notify_usage(cx: &ConnectionTo<Client>, session_id: &SessionId, prompt: &str, assistant: &str) -> Result<(), AcpError> {
    cx.send_notification(SessionNotification::new(session_id.clone(), SessionUpdate::UsageUpdate(usage_update(prompt, assistant))))
}

#[cfg(test)]
#[path = "../test/notify.rs"]
mod tests;
