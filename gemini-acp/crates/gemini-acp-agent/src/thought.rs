//! Heuristique de séparation pensée/réponse pour les modèles thinking.
use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, MessageId, SessionId, SessionNotification, SessionUpdate, TextContent};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

#[derive(Debug, Default)]
pub struct ThoughtSplitter {
    in_thought: bool,
    thought_buffer: String,
    has_emitted_thought: bool,
}

impl ThoughtSplitter {
    pub fn new(is_thinking_model: bool) -> Self { Self { in_thought: is_thinking_model, thought_buffer: String::new(), has_emitted_thought: false } }

    pub fn feed(&mut self, delta: &str) -> (String, String) {
        if !self.in_thought { return (String::new(), delta.to_string()); }
        self.thought_buffer.push_str(delta);
        if let Some(marker_idx) = find_thought_end(&self.thought_buffer) {
            let thought = self.thought_buffer[..marker_idx].to_string();
            let message = self.thought_buffer[marker_idx..].to_string();
            self.thought_buffer.clear(); self.in_thought = false; self.has_emitted_thought = true;
            (thought, message)
        } else { (String::new(), String::new()) }
    }

    pub fn flush(&mut self) -> (String, String) {
        if self.in_thought && !self.thought_buffer.is_empty() {
            let thought = std::mem::take(&mut self.thought_buffer);
            self.in_thought = false; self.has_emitted_thought = true;
            (thought, String::new())
        } else { (String::new(), String::new()) }
    }

    #[allow(dead_code)]
    pub fn has_emitted_thought(&self) -> bool { self.has_emitted_thought }
}

fn find_thought_end(buffer: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(dnl_idx) = buffer[search_from..].find("\n\n") {
        let abs_idx = search_from + dnl_idx;
        let after = &buffer[abs_idx + 2..];
        if after.starts_with("## ") || after.starts_with("# ") || after.starts_with("### ") || after.starts_with("#### ") || (after.starts_with("**") && after.len() > 2 && after[2..].chars().next().is_some_and(|c| c.is_alphanumeric())) { return Some(abs_idx); }
        search_from = abs_idx + 1;
    }
    None
}

pub async fn notify_thought(cx: &ConnectionTo<Client>, session_id: &SessionId, message_id: &MessageId, text: String) -> Result<(), AcpError> {
    if text.is_empty() { return Ok(()); }
    cx.send_notification(SessionNotification::new(session_id.clone(), SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(text))).message_id(message_id.clone()))))
}

#[cfg(test)]
#[path = "../test/thought.rs"]
mod tests;
