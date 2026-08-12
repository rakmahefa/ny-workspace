//! Runtime de l'agent : construction et cycle de vie de l'état applicatif.
use std::sync::Arc;
use std::time::Duration;
use anyhow::{Context, Result};
use gemini_acp_config::{AgentConfig, SettingsManager, SettingsManagerOptions};
use crate::session::SessionManager;
use crate::tools::ToolRegistry;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct AppState { pub store: Arc<crate::state::Store>, pub sessions: SessionManager, pub client: gemini_acp_config::client::Client, pub config: Arc<AgentConfig>, pub settings: Arc<tokio::sync::Mutex<SettingsManager>>, pub tools: Arc<ToolRegistry> }
pub struct AgentRuntime { state: AppState }
impl AgentRuntime {
    pub async fn from_config(config: AgentConfig) -> Result<Self> {
        for warning in config.validate() { tracing::warn!(%warning, "avertissement de configuration"); }
        tokio::fs::create_dir_all(&config.data_dir).await.with_context(|| format!("création {}", config.data_dir.display()))?;
        let store = Arc::new(crate::state::Store::open(&config.data_dir).await.with_context(|| format!("ouverture du store {}", config.data_dir.display()))?);
        let sessions = SessionManager::new(Arc::clone(&store));
        let client = gemini_acp_config::client::Client::new(gemini_acp_config::client::Config { cookie_file: config.cookie_file.clone(), default_model: config.default_model.clone(), auth_user: config.auth_user, proxy: config.proxy.clone(), ..Default::default() }).await.context("initialisation du client Gemini")?;
        let cwd = std::env::current_dir().context("résolution du cwd")?;
        let mut settings = SettingsManager::new(cwd, SettingsManagerOptions::default());
        settings.initialize().await.context("initialisation du SettingsManager")?;
        let mut tools = ToolRegistry::builtin();
        tools.register(Box::new(crate::tools::interactive::AskUserQuestionTool));
        Ok(Self { state: AppState { store, sessions, client, config: Arc::new(config), settings: Arc::new(tokio::sync::Mutex::new(settings)), tools: Arc::new(tools) } })
    }
    pub fn state(&self) -> &AppState { &self.state }
    pub async fn settings(&self) -> serde_json::Value { self.state.settings.lock().await.settings() }
    pub async fn shutdown(&self) {
        let store = Arc::clone(&self.state.store);
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, store.cancel_all()).await { Ok(_) => tracing::info!("tours actifs annulés"), Err(_) => tracing::warn!(timeout_secs = SHUTDOWN_TIMEOUT.as_secs(), "timeout pendant l'arrêt gracieux") }
        self.state.settings.lock().await.dispose().await;
    }
}

#[cfg(test)]
#[path = "../test/runtime.rs"]
mod tests;
