//! Cœur partagé Gemini : cookies Google, auth SAPISIDHASH, parsing du flux
//! Gemini, table des modèles, erreurs typées, utilitaires temps. Aucune
//! dépendance réseau ; pur traitement de données réutilisable par les
//! modules `client`, `web2api`, et les crates `runtime`/`agent` en aval.
//!
//! Anciennement le crate `gemini-core` — déplacé tel quel dans
//! `gemini-acp-config::core` (spec §3.2).

pub mod auth;
pub mod cookies;
pub mod errors;
pub mod frames;
pub mod models;
pub mod time;
pub mod tool_prompt;

pub use auth::sapisid_hash;
pub use cookies::CookieJar;
pub use errors::{GeminiError, GeminiResult};
pub use models::resolve as resolve_model;
pub use time::{now_iso, now_unix, now_unix_u64};
