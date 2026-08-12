//! Module `tools` — architecture d'outils pour l'agent ACP.
//!
//! Conception :
//! - [`executor`]  : exécution, permissions ACP et cycle de vie des tool calls.
//! - [`tool_ux`]   : mapping UX ACP inspiré de `claude-agent-acp/src/tools.ts`.
//! - [`registry`]  : trait `Tool`, `ToolDef`, `ToolRegistry`, `ToolResult`.
//! - [`parse`]     : extraction des blocs `tool_call` depuis la réponse Gemini.
//! - [`prompt`]    : injection `# Tool Use` dans le prompt + formatage historique.
//! - [`sandbox`]   : validation de sécurité (path traversal, shell sandbox).
//! - [`builtin`]   : outils intégrés.
//! - [`interactive`] : outils qui utilisent directement les capacités interactives ACP.

pub mod builtin;
pub mod executor;
pub mod interactive;
pub mod parse;
pub mod prompt;
pub mod registry;
pub mod sandbox;
pub mod tool_ux;

pub use registry::ToolRegistry;