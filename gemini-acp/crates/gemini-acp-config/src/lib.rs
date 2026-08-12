//! `gemini-acp-config` — fondation du workspace `gemini-acp` (spec §3).
//!
//! Contient tout ce qui est en dessous du runtime et qui ne dépend jamais de
//! lui :
//! - [`core`] (anciennement `gemini-core`) : cookies Google, auth
//!   SAPISIDHASH, parsing des frames du flux Gemini, table des modèles,
//!   erreurs typées, utilitaires temps. Pur traitement de données.
//! - [`client`] (anciennement `gemini-client`) : client web
//!   `gemini.google.com` (`StreamGenerate`, streaming, retry, upload
//!   Scotty).
//! - [`web2api`] (anciennement `gemini-web2api`) : proxy HTTP compatible
//!   OpenAI/Google (binaire `gemini-web2api`, voir `src/web2api/main.rs`).
//! - [`config`] : résolution de la configuration depuis l'environnement,
//!   options ACP, capabilities — équivalent du `SettingsManager` de
//!   `vendor/claude-agent-acp/src/settings.ts`.

pub mod client;
pub mod config;
pub mod core;

// NOTE : `web2api` (anciennement le crate `gemini-web2api`) n'est
// volontairement PAS monté comme sous-module de cette bibliothèque. C'est un
// arbre de modules propre au binaire `gemini-web2api`
// (`src/web2api/main.rs`, `[[bin]] path = "src/web2api/main.rs"`), qui
// référence `core`/`client` via le chemin absolu `gemini_acp_config::`
// (dépendance implicite bin → lib du même package). Le fichier
// `src/web2api/mod.rs` documente la disposition cible du spec (§2.2) mais
// n'est délibérément inclus par aucun `mod` — les fichiers de `web2api/`
// utilisent `crate::`/`super::` en supposant `main.rs` comme racine, ce qui
// entre en conflit avec un montage sous `gemini_acp_config::web2api`.

// Re-exports pratiques (compatibilité migration).
pub use client::{Client as GeminiClient, Config as ClientConfig};
pub use config::AgentConfig;
pub use core::models::{resolve as resolve_model, DEFAULT_MODEL};
pub use core::time::{now_iso, now_unix};
pub use core::{sapisid_hash, CookieJar, GeminiError, GeminiResult};
