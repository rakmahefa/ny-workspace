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

/// Avertissement de configuration non bloquant (fichier de cookies absent,
/// etc.) — remonté par `validate()` pour affichage au démarrage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning(pub String);

impl std::fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AgentConfig {
    /// Résout la configuration depuis l'environnement.
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

    /// Vérifications non bloquantes (le fichier de cookies absent
    /// n'empêche pas le démarrage, mais les requêtes Gemini échoueront).
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
mod tests {
    use super::*;

    #[test]
    fn config_is_cloneable_and_preserves_values() {
        let config = AgentConfig {
            cookie_file: PathBuf::from("cookies.json"),
            default_model: "model".into(),
            data_dir: PathBuf::from("/tmp/gemini-acp"),
            auth_user: Some(2),
            proxy: Some("http://proxy".into()),
        };
        let cloned = config.clone();
        assert_eq!(cloned.cookie_file, config.cookie_file);
        assert_eq!(cloned.default_model, config.default_model);
        assert_eq!(cloned.data_dir, config.data_dir);
        assert_eq!(cloned.auth_user, config.auth_user);
        assert_eq!(cloned.proxy, config.proxy);
    }

    #[test]
    fn from_env_defaults_sans_variables() {
        for key in [
            "GEMINI_ACP_COOKIES",
            "GEMINI_ACP_MODEL",
            "GEMINI_ACP_DATA_DIR",
            "XDG_DATA_HOME",
            "GEMINI_ACP_AUTH_USER",
            "GEMINI_ACP_PROXY",
        ] {
            std::env::remove_var(key);
        }
        let config = AgentConfig::from_env();
        assert_eq!(config.cookie_file, PathBuf::from("vendor/cookie.json"));
        assert_eq!(config.default_model, crate::core::models::DEFAULT_MODEL);
        assert_eq!(config.auth_user, None);
        assert_eq!(config.proxy, None);
    }

    #[test]
    fn from_env_lit_les_variables() {
        std::env::set_var("GEMINI_ACP_COOKIES", "/tmp/cookies.json");
        std::env::set_var("GEMINI_ACP_MODEL", "gemini-3.6-flash");
        std::env::set_var("GEMINI_ACP_AUTH_USER", "3");
        std::env::set_var("GEMINI_ACP_PROXY", "http://proxy.local");
        let config = AgentConfig::from_env();
        assert_eq!(config.cookie_file, PathBuf::from("/tmp/cookies.json"));
        assert_eq!(config.default_model, "gemini-3.6-flash");
        assert_eq!(config.auth_user, Some(3));
        assert_eq!(config.proxy.as_deref(), Some("http://proxy.local"));
        for key in [
            "GEMINI_ACP_COOKIES",
            "GEMINI_ACP_MODEL",
            "GEMINI_ACP_AUTH_USER",
            "GEMINI_ACP_PROXY",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn validate_warns_missing_cookies() {
        let config = AgentConfig {
            cookie_file: PathBuf::from("/nonexistent/path/cookies.json"),
            default_model: "model".into(),
            data_dir: PathBuf::from("/tmp/gemini-acp"),
            auth_user: None,
            proxy: None,
        };
        let warnings = config.validate();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].0.contains("cookies"));
    }
}
