//! Builtin tools.
//!
//! Filesystem tools are implemented first and remain on the existing `Tool`
//! trait/registry path. Shell and search stay unchanged until their dedicated
//! migration phases.

pub mod file;
pub mod search;
pub mod shell;
