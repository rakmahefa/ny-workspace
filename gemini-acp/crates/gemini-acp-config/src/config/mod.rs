//! Configuration de l'agent — équivalent Rust du `SettingsManager` de
//! `vendor/claude-agent-acp`. La configuration est résolue une seule fois
//! au démarrage (`AgentConfig::from_env`), puis injectée dans le runtime
//! (`gemini_acp_runtime::AgentRuntime::from_config`). Les modules en aval ne
//! lisent donc jamais directement l'environnement.
//!
//! Refactor 3-crates (spec §3.2) : la construction des dépendances
//! applicatives (`build_state`) ne vit plus ici — elle a été déplacée dans
//! `gemini_acp_runtime::AgentRuntime::from_config`, pour que ce crate reste
//! la fondation (zéro dépendance vers `runtime` ou `agent`).

pub mod config_options;
pub mod env;

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub cookie_file: PathBuf,
    pub default_model: String,
    pub data_dir: PathBuf,
    pub auth_user: Option<u32>,
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning(pub String);

impl std::fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AgentConfig {
    pub fn from_env() -> Self {
        Self {
            cookie_file: env::env_or("GEMINI_ACP_COOKIES", "vendor/cookie.json").into(),
            default_model: env::env_or(
                "GEMINI_ACP_MODEL",
                crate::core::models::DEFAULT_MODEL,
            ),
            data_dir: env::data_dir_default(),
            auth_user: env::parse_auth_user(),
            proxy: std::env::var("GEMINI_ACP_PROXY").ok(),
        }
    }

    pub fn validate(&self) -> Vec<ConfigWarning> {
        let mut warnings = Vec::new();
        if !self.cookie_file.exists() {
            warnings.push(ConfigWarning(format!(
                "fichier de cookies introuvable: {}",
                self.cookie_file.display()
            )));
        }
        warnings
    }
}

#[cfg(test)]
#[path = "../test/config.rs"]
mod tests;
