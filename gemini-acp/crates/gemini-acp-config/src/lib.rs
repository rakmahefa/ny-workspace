//! `gemini-acp-config` — fondation du workspace `gemini-acp` (spec §3).
//!
//! Contient tout ce qui est en dessous du runtime et qui ne dépend jamais de
//! lui : core Gemini, client web, configuration statique et settings dynamiques.

pub mod client;
pub mod config;
pub mod core;
pub mod settings;

// `web2api` reste un binaire séparé afin de conserver sa racine de modules historique.

pub use client::{Client as GeminiClient, Config as ClientConfig};
pub use config::AgentConfig;
pub use core::models::{resolve as resolve_model, DEFAULT_MODEL};
pub use core::time::{now_iso, now_unix};
pub use core::{sapisid_hash, CookieJar, GeminiError, GeminiResult};
pub use settings::{SettingsManager, SettingsManagerOptions};
