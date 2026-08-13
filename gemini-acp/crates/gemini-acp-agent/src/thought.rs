//! Gestion du flux `thinking` Gemini → ACP.
//!
//! Le pattern retenu suit le modèle ACP de `claude-agent-acp` : la pensée et
//! le message utilisateur sont traités comme deux flux logiques distincts et
//! chaque morceau de pensée est immédiatement traduit en
//! `SessionUpdate::AgentThoughtChunk`.
//!
//! Le backend Gemini utilisé par ce projet expose aujourd'hui un flux texte
//! aplati. Tant qu'il ne fournit pas de champ `thought` structuré au niveau du
//! transport, ce module fournit donc un *fallback* déterministe :
//!
//! - les modèles thinking entrent en état `Thinking` ;
//! - `</think>` / `</thinking>` terminent explicitement la pensée ;
//! - les séparateurs Markdown historiques (`#`, `##`, `###`, `####`, `**...**`)
//!   restent supportés pour compatibilité ;
//! - la pensée est émise progressivement avec une petite fenêtre de garde afin
//!   de reconnaître un marqueur qui arrive coupé entre plusieurs deltas ;
//! - après la transition, les deltas sont toujours envoyés directement comme
//!   `agent_message_chunk`.
//!
//! Lorsque Gemini fournira un indicateur de pensée structuré dans son flux web,
//! cette machine d'état pourra être remplacée par un mapping direct sans
//! modifier la couche ACP qui consomme `notify_thought`.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionId, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

/// Longueur maximale conservée afin de reconnaître un marqueur arrivé coupé
/// entre deux deltas réseau. Le plus long marqueur supporté est `</thinking>`.
const MARKER_LOOKBEHIND: usize = 32;

/// État logique du flux retourné par Gemini.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThoughtState {
    Thinking,
    Response,
}

#[derive(Debug, Default)]
pub struct ThoughtSplitter {
    state: ThoughtState,
    pending: String,
    has_emitted_thought: bool,
}

impl Default for ThoughtState {
    fn default() -> Self {
        Self::Response
    }
}

impl ThoughtSplitter {
    /// Construit le parseur. `is_thinking_model` active le mode pensée pour le
    /// tour courant ; un modèle classique reste un simple flux texte.
    pub fn new(is_thinking_model: bool) -> Self {
        Self {
            state: if is_thinking_model {
                ThoughtState::Thinking
            } else {
                ThoughtState::Response
            },
            pending: String::new(),
            has_emitted_thought: false,
        }
    }

    /// Ingest un delta de stream et retourne `(thought_delta, message_delta)`.
    ///
    /// Contrairement à l'ancien splitter, cette fonction n'attend pas toute la
    /// réponse : elle émet immédiatement la majeure partie de la pensée et ne
    /// conserve qu'une petite fenêtre pour détecter les marqueurs inter-deltas.
    pub fn feed(&mut self, delta: &str) -> (String, String) {
        if delta.is_empty() {
            return (String::new(), String::new());
        }

        if self.state == ThoughtState::Response {
            return (String::new(), delta.to_string());
        }

        self.pending.push_str(delta);
        self.consume_open_marker();

        if let Some((idx, marker_len, keep_marker)) = find_thought_end(&self.pending) {
            let thought = self.pending[..idx].to_string();
            let message_start = if keep_marker { idx } else { idx + marker_len };
            let message = self.pending[message_start..].to_string();
            self.pending.clear();
            self.state = ThoughtState::Response;

            if !thought.is_empty() {
                self.has_emitted_thought = true;
            }
            return (thought, message);
        }

        // Flux continu : émettre tout sauf une petite fenêtre de look-behind.
        // Cette fenêtre évite de couper `</think>` ou une variante Markdown
        // juste avant qu'elle ne soit complétée par le delta suivant.
        if self.pending.chars().count() > MARKER_LOOKBEHIND {
            let emit_chars = self.pending.chars().count() - MARKER_LOOKBEHIND;
            let split_at = self
                .pending
                .char_indices()
                .nth(emit_chars)
                .map(|(idx, _)| idx)
                .unwrap_or(self.pending.len());
            let thought = self.pending[..split_at].to_string();
            self.pending.drain(..split_at);
            if !thought.is_empty() {
                self.has_emitted_thought = true;
            }
            return (thought, String::new());
        }

        (String::new(), String::new())
    }

    /// Termine le flux et restitue le reliquat non encore émis.
    pub fn flush(&mut self) -> (String, String) {
        if self.state == ThoughtState::Response {
            let message = std::mem::take(&mut self.pending);
            return (String::new(), message);
        }

        let thought = std::mem::take(&mut self.pending);
        self.state = ThoughtState::Response;
        if !thought.is_empty() {
            self.has_emitted_thought = true;
        }
        (thought, String::new())
    }

    #[allow(dead_code)]
    pub fn has_emitted_thought(&self) -> bool {
        self.has_emitted_thought
    }

    fn consume_open_marker(&mut self) {
        for marker in ["<think>", "<thinking>"] {
            if let Some(rest) = self.pending.strip_prefix(marker) {
                self.pending = rest.to_string();
                break;
            }
        }
    }
}

/// Retourne `(index, longueur_marqueur, garder_marqueur_dans_message)`.
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
            // Pour la compatibilité Markdown on conserve le séparateur dans le
            // message final, comme le faisait la précédente implémentation.
            return Some((abs_idx, 0, true));
        }

        search_from = abs_idx + 2;
    }
    None
}

/// Émet une notification ACP de pensée avec le même `message_id` que le tour.
///
/// Le `message_id` stable permet au client ACP de rattacher les chunks de pensée
/// et les chunks de réponse au même message assistant, à la manière du
/// `applyMessageId` de `claude-agent-acp`.
pub async fn notify_thought(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message_id: &MessageId,
    text: String,
) -> Result<(), AcpError> {
    if text.is_empty() {
        return Ok(());
    }

    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                .message_id(message_id.clone()),
        ),
    ))
}

#[cfg(test)]
#[path = "test/thought.rs"]
mod tests;
