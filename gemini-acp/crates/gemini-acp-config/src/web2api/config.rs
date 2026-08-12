//! Configuration de la face API (spec §5.1) : `config.json` (si présent,
//! cherché dans `./config.json` et `~/.config/gemini-web2api/config.json`,
//! comme `config.py`) puis env `GEMINI_WEB2API_*` (priorité supérieure).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub host: String,
    pub retry_attempts: u32,
    pub retry_delay_sec: u64,
    pub request_timeout_sec: u64,
    pub gemini_bl: String,
    pub auth_user: Option<u32>,
    /// Token `at` forcé (déprécié : le client récupère `SNlM0e` lui-même).
    pub xsrf_token: Option<String>,
    pub default_model: String,
    pub log_requests: bool,
    pub cookie_file: Option<PathBuf>,
    pub proxy: Option<String>,
    /// Clés API optionnelles : si non vide, toute requête `/v1*` doit porter
    /// `Authorization: Bearer <key>`, `x-api-key`/`x-goog-api-key` ou `?key=`.
    pub api_keys: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8081,
            // S10 : on bind par défaut sur 127.0.0.1 (loopback) plutôt que sur
            // 0.0.0.0. Le serveur parle aux cookies Gemini de l'utilisateur —
            // l'exposer sur toutes les interfaces permettrait à un tiers du
            // réseau d'utiliser le quota Gemini de l'utilisateur. Pour exposer
            // le serveur sur le réseau, définir explicitement `host = "0.0.0.0"`
            // dans `config.json` ou `GEMINI_WEB2API_HOST=0.0.0.0`.
            host: "127.0.0.1".into(),
            retry_attempts: 3,
            retry_delay_sec: 2,
            request_timeout_sec: 180,
            gemini_bl: gemini_acp_config::client::DEFAULT_BL.to_string(),
            auth_user: None,
            xsrf_token: None,
            default_model: gemini_acp_config::core::models::DEFAULT_MODEL.to_string(),
            log_requests: true,
            cookie_file: None,
            proxy: None,
            api_keys: Vec::new(),
        }
    }
}

/// Chemins de `config.json` explorés (même ordre que `find_config` du vendor).
fn config_paths() -> [PathBuf; 2] {
    [
        PathBuf::from("./config.json"),
        dirs_config().join("gemini-web2api/config.json"),
    ]
}

fn dirs_config() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".")
}

/// Charge `config.json` (s'il existe) puis surcharge par l'environnement
/// `GEMINI_WEB2API_*`.
pub fn load() -> Config {
    let mut config = Config::default();
    for path in config_paths() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Config>(&raw) {
                Ok(from_file) => {
                    config = from_file;
                    tracing::info!("config chargée depuis {}", path.display());
                }
                Err(e) => tracing::warn!("config.json invalide ({}): {e}", path.display()),
            }
        }
    }
    apply_env(&mut config);
    config
}

fn apply_env(config: &mut Config) {
    let get = |key: &str| std::env::var(format!("GEMINI_WEB2API_{key}")).ok();
    if let Some(v) = get("PORT") {
        if let Ok(n) = v.parse() {
            config.port = n;
        }
    }
    if let Some(v) = get("HOST") {
        config.host = v;
    }
    if let Some(v) = get("RETRY_ATTEMPTS") {
        if let Ok(n) = v.parse() {
            config.retry_attempts = n;
        }
    }
    if let Some(v) = get("RETRY_DELAY_SEC") {
        if let Ok(n) = v.parse() {
            config.retry_delay_sec = n;
        }
    }
    if let Some(v) = get("REQUEST_TIMEOUT_SEC") {
        if let Ok(n) = v.parse() {
            config.request_timeout_sec = n;
        }
    }
    if let Some(v) = get("GEMINI_BL") {
        config.gemini_bl = v;
    }
    if let Some(v) = get("AUTH_USER") {
        config.auth_user = v.parse().ok();
    }
    if let Some(v) = get("XSRF_TOKEN") {
        config.xsrf_token = Some(v);
    }
    if let Some(v) = get("DEFAULT_MODEL") {
        config.default_model = v;
    }
    if let Some(v) = get("LOG_REQUESTS") {
        config.log_requests = v != "0" && v != "false";
    }
    if let Some(v) = get("COOKIE_FILE") {
        config.cookie_file = Some(v.into());
    }
    if let Some(v) = get("PROXY") {
        config.proxy = Some(v);
    }
    if let Some(v) = get("API_KEYS") {
        config.api_keys = v.split(',').map(|s| s.trim().to_string()).collect();
    }
}
