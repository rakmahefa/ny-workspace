//! Runtime de l'agent : construction et cycle de vie de l'état applicatif.
//!
//! Inspiré du rôle de `ClaudeAcpAgent` dans `vendor/claude-agent-acp` : ce
//! module possède le `state::Store`, le `client::Client` Gemini (via
//! `gemini_acp_config`) et le `ToolRegistry`, et orchestre le cycle de vie
//! des sessions.
//!
//! **Principe clé** (spec §4.2) : `AgentRuntime` ne connaît rien du
//! protocole ACP — il est testable sans transport. Le câblage du transport
//! stdio (`run_agent`) vit dans le crate `gemini-acp-agent` et prend
//! `AppState` en paramètre.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use gemini_acp_config::config::AgentConfig;

use crate::tools::ToolRegistry;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// État partagé entre tous les handlers ACP (clonable — `Arc` interne).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<crate::state::Store>,
    pub client: gemini_acp_config::client::Client,
    pub config: Arc<AgentConfig>,
    pub tools: Arc<ToolRegistry>,
}

/// Construit et possède l'état applicatif complet à partir d'une
/// `AgentConfig` déjà résolue.
pub struct AgentRuntime {
    state: AppState,
}

impl AgentRuntime {
    /// Construit toutes les dépendances applicatives (store disque, client
    /// Gemini, registre d'outils) à partir d'une configuration résolue.
    ///
    /// Anciennement `AgentConfig::build_state` (déplacé ici pour que
    /// `gemini-acp-config` reste la fondation sans dépendance vers
    /// `runtime`, spec §3.2 note).
    pub async fn from_config(config: AgentConfig) -> Result<Self> {
        for warning in config.validate() {
            tracing::warn!(%warning, "avertissement de configuration");
        }

        tokio::fs::create_dir_all(&config.data_dir)
            .await
            .with_context(|| format!("création {}", config.data_dir.display()))?;

        let store = Arc::new(
            crate::state::Store::open(&config.data_dir)
                .await
                .with_context(|| format!("ouverture du store {}", config.data_dir.display()))?,
        );

        let client = gemini_acp_config::client::Client::new(gemini_acp_config::client::Config {
            cookie_file: config.cookie_file.clone(),
            default_model: config.default_model.clone(),
            auth_user: config.auth_user,
            proxy: config.proxy.clone(),
            ..Default::default()
        })
        .await
        .context("initialisation du client Gemini")?;

        let tools = Arc::new(ToolRegistry::builtin());

        Ok(Self {
            state: AppState {
                store,
                client,
                config: Arc::new(config),
                tools,
            },
        })
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Annule les tours actifs et attend leur drainage avec une borne stricte.
    pub async fn shutdown(&self) {
        let store = Arc::clone(&self.state.store);
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, store.cancel_all()).await {
            Ok(_) => tracing::info!("tours actifs annulés"),
            Err(_) => tracing::warn!(
                timeout_secs = SHUTDOWN_TIMEOUT.as_secs(),
                "timeout pendant l'arrêt gracieux"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AgentConfig {
        let dir = std::env::temp_dir().join(format!(
            "gemini-acp-runtime-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        AgentConfig {
            cookie_file: dir.join("cookies.json"),
            default_model: gemini_acp_config::core::models::DEFAULT_MODEL.to_string(),
            data_dir: dir.join("data"),
            auth_user: None,
            proxy: None,
        }
    }

    #[tokio::test]
    async fn runtime_from_config_creates_state() {
        let config = test_config();
        let runtime = AgentRuntime::from_config(config).await.expect("runtime");
        assert!(runtime.state().store.list(None).await.is_empty());
    }

    #[tokio::test]
    async fn runtime_shutdown_cancels_all_turns() {
        let config = test_config();
        let runtime = AgentRuntime::from_config(config).await.expect("runtime");
        // Ne doit pas paniquer / bloquer sans tours actifs.
        runtime.shutdown().await;
    }
}
