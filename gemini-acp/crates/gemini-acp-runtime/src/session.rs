//! Cycle de vie ACP des sessions.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::{bail, Context, Result};
use crate::state::{Session, SessionMode, Store};

pub const SESSION_ID_PREFIX: &str = "sess_";
pub const MAX_TITLE_LENGTH: usize = 256;

#[derive(Clone)]
pub struct SessionManager { store: Arc<Store> }
impl SessionManager {
    pub fn new(store: Arc<Store>) -> Self { Self { store } }
    pub fn store(&self) -> &Arc<Store> { &self.store }
    pub fn validate_id(id: &str) -> Result<()> { let Some(rest) = id.strip_prefix(SESSION_ID_PREFIX) else { bail!("identifiant de session invalide: préfixe attendu `{SESSION_ID_PREFIX}`"); }; if rest.len() != 32 || !rest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) { bail!("identifiant de session invalide: UUID hexadécimal minuscule attendu"); } Ok(()) }
    pub async fn validate_cwd(cwd: &Path) -> Result<()> { if !cwd.is_absolute() { bail!("le chemin de session doit être absolu"); } let metadata = tokio::fs::metadata(cwd).await.with_context(|| format!("workspace inaccessible: {}", cwd.display()))?; if !metadata.is_dir() { bail!("le workspace n'est pas un répertoire: {}", cwd.display()); } Ok(()) }
    pub fn sanitize_title(text: &str) -> Option<String> { let title = text.replace(['\r','\n'], " ").split_whitespace().collect::<Vec<_>>().join(" "); if title.is_empty() { return None; } let mut chars = title.chars(); let truncated: String = chars.by_ref().take(MAX_TITLE_LENGTH).collect(); if chars.next().is_some() { let keep = MAX_TITLE_LENGTH.saturating_sub(1); Some(format!("{}…", truncated.chars().take(keep).collect::<String>())) } else { Some(truncated) } }
    pub async fn create(&self, cwd: PathBuf, additional_directories: Vec<PathBuf>, model: &str) -> Result<Session> { Self::validate_cwd(&cwd).await?; for directory in &additional_directories { Self::validate_cwd(directory).await.with_context(|| format!("répertoire additionnel invalide: {}", directory.display()))?; } self.store.create(cwd, additional_directories, model).await.context("création de session") }
    pub async fn get(&self, id: &str) -> Result<Session> { Self::validate_id(id)?; self.store.get(id).await.ok_or_else(|| anyhow::anyhow!("session introuvable: {id}")) }
    pub async fn list(&self, cwd: Option<&Path>) -> Result<Vec<Session>> { if let Some(cwd) = cwd { Self::validate_cwd(cwd).await?; } Ok(self.store.list(cwd).await) }
    pub async fn load(&self, id: &str, cwd: &Path) -> Result<Session> { Self::validate_id(id)?; Self::validate_cwd(cwd).await?; let session = self.get(id).await?; if session.cwd != cwd { bail!("le cwd ne correspond pas à la session"); } Ok(session) }
    pub async fn resume(&self, id: &str, cwd: &Path) -> Result<Session> { self.load(id,cwd).await }
    pub async fn set_title(&self, id: &str, title: &str) -> Result<()> { let title = Self::sanitize_title(title); self.get(id).await?; self.store.update_session(id, move |session| session.title = title).await.context("mise à jour du titre de session") }
    pub async fn set_title_from_prompt(&self, id: &str, prompt: &str) -> Result<()> { let title = Self::sanitize_title(prompt); if title.is_none() { return Ok(()); } self.store.update_session(id, move |session| { if session.title.is_none() { session.title = title; } }).await.context("initialisation du titre de session") }
    pub async fn set_mode(&self, id: &str, mode: SessionMode) -> Result<Session> { let mut updated = self.get(id).await?; self.store.update_session(id, |session| session.mode = mode).await.context("mise à jour du mode de session")?; updated.mode = mode; Ok(updated) }
    pub async fn fork(&self, id: &str) -> Result<Session> { self.get(id).await?; self.store.fork(id).await.context("fork de session") }
    pub async fn close(&self, id: &str) -> Result<bool> { Self::validate_id(id)?; Ok(self.store.close(id).await) }
    pub async fn delete(&self, id: &str) -> Result<bool> { Self::validate_id(id)?; Ok(self.store.delete(id).await) }
    pub async fn cancel(&self, id: &str) -> Result<()> { Self::validate_id(id)?; self.store.cancel(id).await; Ok(()) }
}

#[cfg(test)]
#[path = "../test/session.rs"]
mod tests;
