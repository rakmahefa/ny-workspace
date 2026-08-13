//! Encapsulation du flux `thinking` Gemini → ACP.
//!
//! Le parseur est volontairement indépendant du transport Gemini et de la couche
//! ACP. Il transforme un flux de deltas en événements sémantiques : pensée,
//! transition vers la réponse, ou réponse.
//!
//! Cette séparation suit le principe de `claude-agent-acp` : le cycle d'un tour
//! est piloté par un état explicite, plutôt que par un simple post-traitement du
//! texte produit. Le consommateur ACP n'a donc pas à connaître les marqueurs
//! Gemini ni à mélanger thinking et réponse.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionId, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

/// Fenêtre maximale conservée pour reconnaître un marqueur arrivé coupé entre
/// plusieurs deltas réseau. Le plus long marqueur supporté est `</thinking>`.
const MARKER_LOOKBEHIND: usize = 32;

/// Phase sémantique du flux d'un tour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThoughtPhase {
    Response,
    Thinking,
    Completed,
}

/// Événement produit par [`ThoughtStream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThoughtEvent {
    ThoughtChunk(String),
    ThoughtEnd,
    ResponseChunk(String),
}

/// Adaptateur du flux texte Gemini vers un flux d'événements sémantiques.
#[derive(Debug)]
pub struct ThoughtStream {
    phase: ThoughtPhase,
    pending: String,
    emitted_thought: bool,
}

impl ThoughtStream {
    /// Initialise le flux. Les modèles non-thinking sont directement en réponse.
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

    /// Ingère un delta et retourne uniquement les événements causés par ce delta.
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

    /// Termine le flux. Le reliquat d'une pensée ouverte reste une pensée ; la
    /// couche de tour décide ensuite si l'absence de réponse constitue une
    /// divergence ou une réponse valide.
    pub fn finish(&mut self) -> Vec<ThoughtEvent> {
        if self.phase == ThoughtPhase::Completed {
            return Vec::new();
        }

        let mut events = Vec::new();
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            self.emitted_thought = true;
            if self.phase == ThoughtPhase::Thinking {
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

/// Émet un chunk ACP de pensée avec le même `message_id` que le tour.
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
