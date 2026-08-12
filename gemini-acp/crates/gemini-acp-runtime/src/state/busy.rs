//! Gestion du sentinel inter-processus `busy` : fichier `<id>.busy` cohabitant
//! avec `<id>.json`. Présence = un tour est en cours (soit dans ce processus,
//! soit dans un autre process `gemini-acp` / `gemini-acp-snapshot`).
//!
//! Utilisé pour éviter que `gemini-acp-snapshot restore` n'écrase une
//! session pendant qu'un agent est en plein tour (data corruption).

use super::Store;

impl Store {
    /// Chemin du sentinel `busy`.
    pub(crate) fn busy_path(&self, id: &str) -> std::path::PathBuf {
        self.dir.join(format!("{id}.busy"))
    }

    /// Crée atomiquement le sentinel `busy` (échoue si déjà présent).
    ///
    /// `create_new(true)` => atomicité POSIX : si le fichier existe déjà,
    /// on obtient `AlreadyExists`. On le considère alors comme résidu d'un
    /// crash passé et on l'écrase.
    pub(crate) async fn acquire_busy(&self, id: &str) -> anyhow::Result<()> {
        let path = self.busy_path(id);
        let content = format!(
            "pid={} ts={}\n",
            std::process::id(),
            gemini_acp_config::core::time::now_unix()
        );
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(_) => {
                let _ = tokio::fs::write(&path, &content).await;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = tokio::fs::write(&path, format!("{content}(re-acquired)\n")).await;
            }
            Err(e) => {
                tracing::warn!(
                    "impossible de créer le sentinel busy {}: {e}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    /// Supprime le sentinel `busy`. Appelée à `end_turn` / `cancel` / `TurnGuard::drop`.
    pub async fn release_busy(&self, id: &str) {
        let _ = tokio::fs::remove_file(self.busy_path(id)).await;
    }

    /// Force la session à l'état inactif : libère le flag mémoire `busy` et
    /// le sentinel disque.
    ///
    /// Méthode publique car `live` est `pub(crate)` — inaccessible depuis
    /// le binaire `gemini-acp` qui consomme le crate lib `acp`.
    pub async fn force_idle(&self, id: &str) {
        if let Some(entry) = self.live.write().await.get_mut(id) {
            entry.busy = false;
        }
        self.release_busy(id).await;
    }
}
