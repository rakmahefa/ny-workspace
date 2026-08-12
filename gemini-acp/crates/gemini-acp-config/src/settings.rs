//! Gestionnaire de configuration dynamique inspiré de `claude-agent-acp/src/settings.ts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Map, Value};
use tokio::sync::Notify;

const DEBOUNCE: Duration = Duration::from_millis(100);

pub struct SettingsManagerOptions {
    pub on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for SettingsManagerOptions {
    fn default() -> Self { Self { on_change: None } }
}

pub struct SettingsManager {
    cwd: PathBuf,
    effective: Arc<Mutex<Value>>,
    watcher: Option<RecommendedWatcher>,
    reload_signal: Arc<Notify>,
    shutdown: Arc<Notify>,
    reload_task: Option<tokio::task::JoinHandle<()>>,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl SettingsManager {
    pub fn new(cwd: impl Into<PathBuf>, options: SettingsManagerOptions) -> Self {
        Self {
            cwd: cwd.into(),
            effective: Arc::new(Mutex::new(Value::Object(Map::new()))),
            watcher: None,
            reload_signal: Arc::new(Notify::new()),
            shutdown: Arc::new(Notify::new()),
            reload_task: None,
            on_change: options.on_change,
        }
    }

    pub async fn initialize(&mut self) -> Result<()> {
        self.load().await?;
        self.setup_watchers()?;
        self.start_reload_loop();
        Ok(())
    }

    pub fn settings(&self) -> Value {
        self.effective.lock().expect("settings mutex poisoned").clone()
    }

    pub fn cwd(&self) -> &Path { &self.cwd }

    pub async fn set_cwd(&mut self, cwd: impl Into<PathBuf>) -> Result<()> {
        let cwd = cwd.into();
        if self.cwd == cwd { return Ok(()); }
        self.dispose().await;
        self.cwd = cwd;
        self.initialize().await
    }

    pub async fn dispose(&mut self) {
        self.shutdown.notify_waiters();
        self.reload_signal.notify_waiters();
        if let Some(task) = self.reload_task.take() { let _ = task.await; }
        self.watcher = None;
    }

    fn watched_files(&self) -> Vec<PathBuf> {
        let user_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|dir| dir.join("gemini-acp/settings.json"));
        let mut paths = Vec::new();
        if let Some(path) = user_config { paths.push(path); }
        paths.push(self.cwd.join(".gemini/settings.json"));
        paths.push(self.cwd.join(".gemini/settings.local.json"));
        paths.push(PathBuf::from("/etc/gemini-acp/managed-settings.json"));
        paths
    }

    async fn load(&self) -> Result<()> {
        let effective = load_settings(&self.watched_files()).await?;
        let mut current = self.effective.lock().expect("settings mutex poisoned");
        if *current != effective {
            *current = effective;
            if let Some(callback) = &self.on_change { callback(); }
        }
        Ok(())
    }

    fn setup_watchers(&mut self) -> Result<()> {
        let watched = self.watched_files();
        let watched_names: Arc<HashSet<PathBuf>> = Arc::new(watched.iter().cloned().collect());
        let signal = Arc::clone(&self.reload_signal);
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                let Ok(event) = result else { return; };
                if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)) { return; }
                if event.paths.iter().any(|path| watched_names.contains(path)) { signal.notify_one(); }
            },
            Config::default(),
        )?;

        let mut directories = HashSet::new();
        for path in watched {
            if let Some(parent) = path.parent() { directories.insert(parent.to_path_buf()); }
        }
        for directory in directories {
            if directory.exists() { watcher.watch(&directory, RecursiveMode::NonRecursive)?; }
        }
        self.watcher = Some(watcher);
        Ok(())
    }

    fn start_reload_loop(&mut self) {
        let signal = Arc::clone(&self.reload_signal);
        let shutdown = Arc::clone(&self.shutdown);
        let effective = Arc::clone(&self.effective);
        let watched = self.watched_files();
        let callback = self.on_change.clone();
        self.reload_task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.notified() => break,
                    _ = signal.notified() => {
                        tokio::time::sleep(DEBOUNCE).await;
                        match load_settings(&watched).await {
                            Ok(next) => {
                                let mut current = effective.lock().expect("settings mutex poisoned");
                                if *current != next {
                                    *current = next;
                                    if let Some(callback) = &callback { callback(); }
                                }
                            }
                            Err(error) => tracing::warn!(%error, "rechargement des settings impossible"),
                        }
                    }
                }
            }
        }));
    }
}

async fn load_settings(paths: &[PathBuf]) -> Result<Value> {
    let mut effective = Value::Object(Map::new());
    for path in paths {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => {
                let value: Value = serde_json::from_str(&content)
                    .with_context(|| format!("settings JSON invalide: {}", path.display()))?;
                merge_json(&mut effective, value);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(path = %path.display(), %error, "lecture des settings impossible"),
        }
    }
    Ok(effective)
}

fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => { base.insert(key, value); }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_is_recursive_and_overlay_wins() {
        let mut base = serde_json::json!({"model":"flash","permissions":{"read":true,"write":false}});
        merge_json(&mut base, serde_json::json!({"permissions":{"write":true},"tools":true}));
        assert_eq!(base["model"], "flash");
        assert_eq!(base["permissions"]["read"], true);
        assert_eq!(base["permissions"]["write"], true);
        assert_eq!(base["tools"], true);
    }

    #[tokio::test]
    async fn manager_can_initialize_without_config_files() {
        let mut manager = SettingsManager::new("/definitely/nonexistent/gemini-acp-test", SettingsManagerOptions::default());
        manager.initialize().await.expect("initialize");
        assert_eq!(manager.settings(), serde_json::json!({}));
        manager.dispose().await;
    }
}
