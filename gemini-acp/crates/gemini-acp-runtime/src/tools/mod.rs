//! Module `tools` — architecture d'outils pour l'agent ACP.
//!
//! Conception :
//! - [`executor`]  : exécution, permissions ACP et cycle de vie des tool calls.
//! - [`lifecycle`] : machine d'état déterministe interne, projetée sur les statuts ACP v1.
//! - [`request`]   : modèle normalisé `ToolCallRequest`, son type et sa machine d'état.
//! - [`tool_ux`]   : mapping UX ACP inspiré de `claude-agent-acp/src/tools.ts`.
//! - [`elicitation`] : projection structurée des questions utilisateur vers ACP.
//! - [`registry`]  : trait `Tool`, `ToolDef`, `ToolRegistry`, `ToolResult`.
//! - [`parse`]     : extraction des blocs `tool_call` depuis la réponse Gemini.
//! - [`prompt`]    : injection `# Tool Use` dans le prompt + formatage historique.
//! - [`sandbox`]   : validation de sécurité (path traversal, shell sandbox).
//! - [`builtin`]   : outils intégrés.
//! - [`interactive`] : façade stable vers l'implémentation interactive ACP.

pub mod builtin;
pub mod elicitation;
pub mod executor;
mod interactive_v2;
pub use interactive_v2 as interactive;
pub mod lifecycle;
pub mod parse;
pub mod prompt;
pub mod registry;
pub mod request;
pub mod sandbox;
pub mod tool_ux;

pub use lifecycle::{LifecycleError, ToolLifecycle, ToolLifecycleState};
pub use registry::ToolRegistry;
pub use request::{ToolCallKind, ToolCallRequest, ToolCallRequestError, ToolCallState};
