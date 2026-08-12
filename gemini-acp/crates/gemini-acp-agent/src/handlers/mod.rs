//! Handlers ACP par méthode (refactor M9 §6.1).
//!
//! Chaque handler est une fonction `pub async fn` testable indépendamment,
//! prenant un `&AppState` partagé au lieu de cloner `store`/`client` à chaque
//! closure. Le module `agent` est responsable du câblage (`Agent.builder()`).

pub mod cancel;
pub mod config;
pub mod init;
pub mod session;
