//! Module `tools` — architecture d'outils pour l'agent ACP.
//!
//! Refactor R1 : ajout de [`executor`] inspiré de `glm-acp-agent/src/tools/executor.ts`.
//!
//! Conception :
//! - [`executor`]  : `ToolExecutor` avec dispatch, permissions, notifications ACP.
//! - [`registry`]  : trait `Tool`, `ToolDef`, `ToolRegistry`, `ToolResult`.
//! - [`parse`]     : extraction des blocs `tool_call` depuis la réponse Gemini.
//! - [`prompt`]    : injection `# Tool Use` dans le prompt + formatage historique.
//! - [`sandbox`]   : validation de sécurité (path traversal, shell sandbox).
//! - [`builtin`]   : outils intégrés (file_read, file_write, shell_exec, search).

pub mod builtin;
pub mod executor;
pub mod parse;
pub mod prompt;
pub mod registry;
pub mod sandbox;

// Re-exports pour usage pratique.
pub use registry::ToolRegistry;

