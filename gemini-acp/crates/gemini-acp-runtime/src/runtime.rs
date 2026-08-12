//! Runtime de l'agent : construction et cycle de vie de l'état applicatif.
//!
//! Inspiré du rôle de `ClaudeAcpAgent` dans `vendor/claude-agent-acp` : ce
//! module possède le `state::Store`, le `client::Client` Gemini, le registre
//! d'outils et maintenant le `SettingsManager` dynamique.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use gemini_acp_config::{AgentConfig, SettingsManager, SettingsManagerOptions};

use crate::tools::ToolRegistry;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// État partagé entre tous les handlers ACP.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<crate::state::Store>,
    pub client: gemini_acp_config::client::Client,
    pub config: Arc<AgentConfig>,
    pub settings: Arc<tokio::sync::Mutex<SettingsManager>>,
    pub tools: Arc<ToolRegistry>,
}

/// Construit et possède l'état applicatif complet à partir d'une
/// `AgentConfig` déjà résolue.
pub struct AgentRuntime {
    state: AppState,
}

impl AgentRuntime {
    /// Construit toutes les dépendances applicatives.
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

        // Le SettingsManager devient une dépendance réelle du runtime : il
        // observe le workspace dès le démarrage et conserve la configuration
        // effective disponible aux handlers sans redémarrer l'agent.
        let cwd = std::env::current_dir().context("résolution du cwd")?;
        let mut settings = SettingsManager::new(cwd, SettingsManagerOptions::default());
        settings
            .initialize()
            .await
            .context("initialisation du SettingsManager")?;

        let mut tools = ToolRegistry::builtin();
        // AskUserQuestion est un builtin du runtime, mais son exécution est
        // reliée au client ACP via un contexte task-local installé par
        // `gemini-acp-agent` pour chaque tour.
        tools.register(Box::new(crate::tools::interactive::AskUserQuestionTool));
        let tools = Arc::new(tools);

        Ok(Self {
            state: AppState {
                store,
                client,
                config: Arc::new(config),
                settings: Arc::new(tokio::sync::Mutex::new(settings)),
                tools,
            },
        })
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Snapshot cohérent de la configuration dynamique effective.
    pub async fn settings(&self) -> serde_json::Value {
        self.state.settings.lock().await.settings()
    }

    /// Annule les tours actifs et libère aussi les ressources du
    /// SettingsManager avec une borne stricte.
    pub async fn shutdown(&self) {
        let store = Arc::clone(&self.state.store);
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, store.cancel_all()).await {
            Ok(_) => tracing::info!("tours actifs annulés"),
            Err(_) => tracing::warn!(
                timeout_secs = SHUTDOWN_TIMEOUT.as_secs(),
                "timeout pendant l'arrêt gracieux"
            ),
        }

        let mut settings = self.state.settings.lock().await;
        settings.dispose().await;
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
    async fn runtime_from_config_creates_state_and_settings_manager() {
        let config = test_config();
        let runtime = AgentRuntime::from_config(config).await.expect("runtime");
        assert!(runtime.state().store.list(None).await.is_empty());
        assert!(runtime.settings().await.is_object());
        let names = runtime.state().tools.definitions();
        assert!(names.iter().any(|tool| tool["name"] == "AskUserQuestion"));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn runtime_shutdown_is_safe_without_active_turns() {
        let config = test_config();
        let runtime = AgentRuntime::from_config(config).await.expect("runtime");
        runtime.shutdown().await;
    }
}
