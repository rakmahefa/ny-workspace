//! `gemini-acp-agent` — transport ACP et handlers de protocole (spec §5).
//!
//! Câblage qui connecte le `gemini_acp_runtime::AgentRuntime` (via son
//! `AppState`) au transport stdio via le SDK `agent-client-protocol`.
//!
//! Inspiré de `claude-agent-acp/src/acp-agent.ts` (routage des méthodes ACP,
//! consommateur de stream et primitives utilitaires).

pub mod agent;
pub mod handlers;
pub mod prompt;
pub mod thought;
pub mod utils;
#[cfg(feature = "elicitation")]
pub mod elicitation;

pub use agent::run_agent;
pub use utils::{sleep, Pushable};
