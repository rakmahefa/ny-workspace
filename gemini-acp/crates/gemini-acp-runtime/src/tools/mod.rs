//! Module `tools` — architecture d'outils pour l'agent ACP.
//!
//! Conception :
//! - [`executor`] : `ToolExecutor` avec dispatch, permissions ACP et notifications.
//! - [`registry`] : trait `Tool`, `ToolDef`, `ToolRegistry`, `ToolResult`.
//! - [`parse`] : extraction des blocs `tool_call` depuis la réponse Gemini.
//! - [`prompt`] : injection `# Tool Use` dans le prompt + formatage historique.
//! - [`sandbox`] : validation de sécurité (path traversal, shell sandbox).
//! - [`builtin`] : outils intégrés.
//! - [`interactive`] : outils qui utilisent directement les capacités interactives ACP.

pub mod builtin;
#[path = "executor_real.rs"]
pub mod executor;
pub mod interactive;
pub mod parse;
pub mod prompt;
pub mod registry;
pub mod sandbox;

pub use registry::ToolRegistry;
