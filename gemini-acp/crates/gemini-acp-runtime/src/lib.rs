//! `gemini-acp-runtime` — cœur applicatif de l'agent ACP (spec §4).
//!
//! Construit et gère l'état applicatif : [`state::Store`] (persistance des
//! sessions), le client Gemini (via `gemini_acp_config::client::Client`),
//! [`tools::ToolRegistry`] et [`persona`] (prompts système). C'est le cœur
//! de l'application, entre la config statique (`gemini-acp-config`) et le
//! protocole ACP (`gemini-acp-agent`).
//!
//! Inspiré du rôle du `ClaudeAcpAgent` dans
//! `vendor/claude-agent-acp/src/acp-agent.ts`, qui possède le
//! `SettingsManager`, la `Query` SDK, et orchestre le cycle de vie des
//! sessions.
//!
//! **Principe clé** : [`runtime::AgentRuntime`] ne connaît rien du protocole
//! ACP — testable sans transport.

pub mod persona;
pub mod runtime;
pub mod state;
pub mod tools;

pub use runtime::{AgentRuntime, AppState};
pub use tools::ToolRegistry;
