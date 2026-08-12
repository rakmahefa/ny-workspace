//! Outils builtin : file_read, file_write, shell_exec, search.
//!
//! Chaque outil est une struct unitaire implémentant `tools::registry::Tool`.
//! Les paramètres sont limités pour rester cohérent avec un agent de codage.

pub mod file;
pub mod search;
pub mod shell;
