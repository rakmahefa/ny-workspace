//! Gestion des snapshots de session : création, liste, prune, restauration.
//!
//! Chaque snapshot est un fichier `<id>.<n>.snap.json` dans le dépôt de
//! sessions, où `n` est le nombre de messages avant la fin du tour.
//! Les snapshots sont créés par `end_turn` AVANT la persistance, puis
//! élagués pour ne garder que les `MAX_SNAPSHOTS` plus récents.

use anyhow::{bail, Context, Result};
use tokio::fs;

use super::Store;

impl Store {
    /// Chemin d'un snapshot `<id>.<n>.snap.json`.
    pub(crate) fn snapshot_path(&self, id: &str, n: usize) -> std::path::PathBuf {
        self.dir.join(format!("{id}.{n}.snap.json"))
    }

    /// Garde seulement les `keep` snapshots les plus récents (par n décroissant).
    pub(super) async fn prune_snapshots(&self, id: &str, keep: usize) {
        let snaps = self.list_snapshots(id).await;
        if snaps.len() <= keep {
            return;
        }
        for n in &snaps[keep..] {
            let _ = fs::remove_file(self.snapshot_path(id, *n)).await;
        }
    }

    /// Liste les numéros de snapshots disponibles pour une session (décroissant).
    pub async fn list_snapshots(&self, id: &str) -> Vec<usize> {
        let prefix = format!("{id}.");
        let suffix = ".snap.json";
        let mut snaps = Vec::new();
        let Ok(mut entries) = fs::read_dir(&self.dir).await else {
            return snaps;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stripped) = name
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(suffix))
            {
                if let Ok(n) = stripped.parse::<usize>() {
                    snaps.push(n);
                }
            }
        }
        snaps.sort_by(|a, b| b.cmp(a)); // décroissant
        snaps
    }

    /// Restaure un snapshot par numéro de tour.
    /// La session est remplacée par l'état du snapshot, puis persistée.
    ///
    /// Refuse de restaurer si un tour est en cours sur cette session
    /// (sentinel `.busy` présent). Pour forcer, passer `force = true`.
    pub async fn restore_snapshot(&self, id: &str, turn: usize) -> Result<()> {
        self.restore_snapshot_impl(id, turn, false).await
    }

    /// Variante avec `--force` pour bypasser le check du sentinel `busy`.
    pub async fn restore_snapshot_force(&self, id: &str, turn: usize) -> Result<()> {
        self.restore_snapshot_impl(id, turn, true).await
    }

    async fn restore_snapshot_impl(&self, id: &str, turn: usize, force: bool) -> Result<()> {
        if !force && self.busy_path(id).exists() {
            bail!(
                "un tour est en cours sur la session {id} (sentinel .busy présent). \
                 Arrêtez l'agent ou envoyez session/cancel, ou utilisez --force si l'agent a crashé."
            );
        }
        let snap_path = self.snapshot_path(id, turn);
        let raw = fs::read_to_string(&snap_path)
            .await
            .with_context(|| format!("snapshot introuvable: {}", snap_path.display()))?;
        let session: super::types::Session = serde_json::from_str(&raw)
            .with_context(|| format!("snapshot invalide: {}", snap_path.display()))?;
        self.end_turn(id, session, 0).await
    }
}
