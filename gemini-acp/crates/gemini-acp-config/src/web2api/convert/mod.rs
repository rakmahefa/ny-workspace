//! Conversion des formats d'entrée OpenAI/Google vers le prompt texte Gemini
//! (refactor M9 §6.4 — éclatement de l'ancien `convert.rs` de 710 lignes en
//! sous-modules par format cible).
//!
//! - [`common`] : helpers partagés (usage, résolution stricte du modèle,
//!   `ToolChoice`, `tool_call_block`, `parse_tool_calls`, `warn_xsrf_ignored`).
//! - [`openai`] : `messages_to_prompt` (format OpenAI `/v1/chat/completions`).
//! - [`codex`] : `responses_input_to_messages`, `normalize_responses_tools`
//!   (format Codex CLI `/v1/responses`).
//! - [`google`] : `google_contents_to_prompt`, `parse_google_function_calls`
//!   (format Google natif `/v1beta/models`).

pub mod codex;
pub mod common;
pub mod google;
pub mod openai;

// Re-exports pour compatibilité ascendante (les handlers `chat.rs`,
// `responses.rs`, `google.rs` utilisent `convert::messages_to_prompt` etc.).
pub use codex::{normalize_responses_tools, responses_input_to_messages};
pub use common::{parse_tool_calls, resolve_model_strict, usage, warn_xsrf_ignored, ToolChoice};
pub use google::{google_contents_to_prompt, parse_google_function_calls};
pub use openai::messages_to_prompt;

#[cfg(test)]
mod tests {
    // Les tests sont dans les sous-modules respectifs.
}
