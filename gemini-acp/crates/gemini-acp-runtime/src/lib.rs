//! `gemini-acp-runtime` — cœur applicatif de l'agent ACP (spec §4).
//!
//! Construit et gère l'état applicatif : [`state::Store`] (persistance des
//! sessions), le client Gemini, [`session::SessionManager`], les outils et
//! les prompts système. Le runtime reste indépendant du transport ACP.

pub mod persona;
pub mod runtime;
pub mod session;
pub mod state;
pub mod tools;

pub use runtime::{AgentRuntime, AppState};
pub use session::SessionManager;
pub use tools::ToolRegistry;
