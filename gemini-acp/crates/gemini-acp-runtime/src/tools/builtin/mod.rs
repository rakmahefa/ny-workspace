//! Builtin tools.
//!
//! Filesystem, shell, search, and composed tools all implement the same
//! `Tool` trait and are registered through the existing `ToolRegistry`.

pub mod composed;
pub mod file;
pub mod filesystem;
pub mod search;
pub mod shell;
