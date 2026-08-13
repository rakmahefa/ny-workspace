//! Encapsulation du flux `thinking` Gemini → ACP.
//!
//! Le parseur est indépendant du transport Gemini et de la couche ACP. Il
//! transforme un flux de deltas en événements sémantiques : pensée,
//! transition vers la réponse, ou réponse.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionId, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

const MARKER_LOOKBEHIND: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThoughtPhase {
    Response,
    Thinking,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThoughtEvent {
    ThoughtChunk(String),
    ThoughtEnd,
    ResponseChunk(String),
}

#[derive(Debug)]
pub struct ThoughtStream {
    phase: ThoughtPhase,
    pending: String,
    emitted_thought: bool,
}

impl ThoughtStream {
    pub fn new(is_thinking_model: bool) -> Self {
        Self {
            phase: if is_thinking_model {
                ThoughtPhase::Thinking
            } else {
                ThoughtPhase::Response
            },
            pending: String::new(),
            emitted_thought: false,
        }
    }

    pub fn phase(&self) -> ThoughtPhase {
        self.phase
    }

    pub fn has_emitted_thought(&self) -> bool {
        self.emitted_thought
    }

    pub fn feed(&mut self, delta: &str) -> Vec<ThoughtEvent> {
        if delta.is_empty() || self.phase == ThoughtPhase::Completed {
            return Vec::new();
        }

        if self.phase == ThoughtPhase::Response {
            return vec![ThoughtEvent::ResponseChunk(delta.to_owned())];
        }

        self.pending.push_str(delta);
        self.consume_open_marker();

        if let Some((idx, marker_len, keep_marker)) = find_thought_end(&self.pending) {
            let thought = self.pending[..idx].to_owned();
            let message_start = if keep_marker { idx } else { idx + marker_len };
            let message = self.pending[message_start..].to_owned();
            self.pending.clear();
            self.phase = ThoughtPhase::Response;

            let mut events = Vec::with_capacity(3);
            if !thought.is_empty() {
                self.emitted_thought = true;
                events.push(ThoughtEvent::ThoughtChunk(thought));
            }
            events.push(ThoughtEvent::ThoughtEnd);
            if !message.is_empty() {
                events.push(ThoughtEvent::ResponseChunk(message));
            }
            return events;
        }

        if self.pending.chars().count() > MARKER_LOOKBEHIND {
            let emit_chars = self.pending.chars().count() - MARKER_LOOKBEHIND;
            let split_at = self
                .pending
                .char_indices()
                .nth(emit_chars)
                .map(|(idx, _)| idx)
                .unwrap_or(self.pending.len());
            let thought = self.pending[..split_at].to_owned();
            self.pending.drain(..split_at);
            if !thought.is_empty() {
                self.emitted_thought = true;
                return vec![ThoughtEvent::ThoughtChunk(thought)];
            }
        }

        Vec::new()
    }

    pub fn finish(&mut self) -> Vec<ThoughtEvent> {
        if self.phase == ThoughtPhase::Completed {
            return Vec::new();
        }

        let mut events = Vec::new();
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            if self.phase == ThoughtPhase::Thinking {
                self.emitted_thought = true;
                events.push(ThoughtEvent::ThoughtChunk(pending));
            } else {
                events.push(ThoughtEvent::ResponseChunk(pending));
            }
        }

        if self.phase == ThoughtPhase::Thinking {
            events.push(ThoughtEvent::ThoughtEnd);
        }
        self.phase = ThoughtPhase::Completed;
        events
    }

    fn consume_open_marker(&mut self) {
        for marker in ["<think>", "<thinking>"] {
            if let Some(rest) = self.pending.strip_prefix(marker) {
                self.pending = rest.to_owned();
                break;
            }
        }
    }
}

/// Compatibilité temporaire pour les consommateurs existants.
///
/// La nouvelle orchestration doit consommer `ThoughtStream` directement. Ce
/// wrapper conserve toutefois le contrat historique `(thought, message)` afin
/// que la migration puisse être faite sans rupture intermédiaire.
#[derive(Debug)]
pub struct ThoughtSplitter {
    stream: ThoughtStream,
}

impl ThoughtSplitter {
    pub fn new(is_thinking_model: bool) -> Self {
        Self {
            stream: ThoughtStream::new(is_thinking_model),
        }
    }

    pub fn feed(&mut self, delta: &str) -> (String, String) {
        let mut thought = String::new();
        let mut message = String::new();
        for event in self.stream.feed(delta) {
            match event {
                ThoughtEvent::ThoughtChunk(text) => thought.push_str(&text),
                ThoughtEvent::ResponseChunk(text) => message.push_str(&text),
                ThoughtEvent::ThoughtEnd => {}
            }
        }
        (thought, message)
    }

    pub fn flush(&mut self) -> (String, String) {
        let mut thought = String::new();
        let mut message = String::new();
        for event in self.stream.finish() {
            match event {
                ThoughtEvent::ThoughtChunk(text) => thought.push_str(&text),
                ThoughtEvent::ResponseChunk(text) => message.push_str(&text),
                ThoughtEvent::ThoughtEnd => {}
            }
        }
        (thought, message)
    }

    pub fn has_emitted_thought(&self) -> bool {
        self.stream.has_emitted_thought()
    }
}

fn find_thought_end(buffer: &str) -> Option<(usize, usize, bool)> {
    for marker in ["</thinking>", "</think>"] {
        if let Some(idx) = buffer.find(marker) {
            return Some((idx, marker.len(), false));
        }
    }

    let mut search_from = 0usize;
    while let Some(dnl_idx) = buffer[search_from..].find("\n\n") {
        let abs_idx = search_from + dnl_idx;
        let after = &buffer[abs_idx + 2..];
        let heading = after.starts_with("# ")
            || after.starts_with("## ")
            || after.starts_with("### ")
            || after.starts_with("#### ");
        let bold_label = after.starts_with("**")
            && after.len() > 2
            && after[2..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric());

        if heading || bold_label {
            return Some((abs_idx, 0, true));
        }
        search_from = abs_idx + 2;
    }
    None
}

pub async fn notify_thought(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message_id: &MessageId,
    text: &str,
) -> Result<(), AcpError> {
    if text.is_empty() {
        return Ok(());
    }

    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_owned())))
                .message_id(message_id.clone()),
        ),
    ))
}

#[cfg(test)]
#[path = "test/thought.rs"]
mod tests;
