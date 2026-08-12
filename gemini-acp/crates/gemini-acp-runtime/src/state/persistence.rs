//! Persistance disque des sessions : lecture, écriture atomique, CRUD,
//! nettoyage des fichiers orphelins au démarrage.
//!
//! Écriture atomique : `<id>.json.tmp` + `rename` (POSIX atomique) pour
//! résister aux crashes mid-write.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs;

use super::types::{Live, Session};
use super::Store;

impl Store {
    /// Chemin du fichier JSON d'une session.
    pub(super) fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Lecture d'une session depuis le disque.
    pub(super) async fn read(&self, id: &str) -> Option<Session> {
        let raw = fs::read_to_string(self.path(id)).await.ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Écriture atomique : tmp + rename (POSIX atomique sur même filesystem).
    pub(super) async fn persist(&self, session: &Session) -> Result<()> {
        let raw = serde_json::to_string_pretty(session)?;
        let path = self.path(&session.id);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &raw)
            .await
            .with_context(|| format!("écriture tmp {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .await
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Ouvre (et crée) le dépôt de sessions sous `data_dir/sessions/`.
    /// Nettoie les `.tmp` et `.busy` orphelins (crashes passés) au passage.
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("sessions");
        fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("création du dépôt {}", dir.display()))?;

        // Cleanup des .tmp orphelins (écritures interrompues par crash).
        let mut cleaned = 0;
        if let Ok(mut entries) = fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "tmp") {
                    let _ = fs::remove_file(&path).await;
                    cleaned += 1;
                    tracing::info!(path = %path.display(), "nettoyage .tmp orphelin");
                }
            }
        }
        if cleaned > 0 {
            tracing::info!(count = cleaned, "nettoyage .tmp orphelins terminé");
        }

        // Cleanup des .busy orphelins : un agent qui crash en plein tour
        // laisse le sentinel derrière lui. Au prochain démarrage, on les purge.
        let mut busy_cleaned = 0;
        if let Ok(mut entries) = fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "busy") {
                    let _ = fs::remove_file(&path).await;
                    busy_cleaned += 1;
                    tracing::info!(path = %path.display(), "nettoyage .busy orphelin");
                }
            }
        }
        if busy_cleaned > 0 {
            tracing::info!(count = busy_cleaned, "nettoyage .busy orphelins terminé");
        }

        Ok(Self {
            dir,
            live: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Crée une session (id `sess_<uuid sans tirets>`), persistée immédiatement.
    pub async fn create(
        &self,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        model: &str,
    ) -> Result<Session> {
        let id = format!("sess_{}", uuid::Uuid::new_v4().simple());
        let session = Session::new(id.clone(), cwd, additional_directories, model);
        let (cancel, _) = tokio::sync::watch::channel(false);
        self.persist(&session).await?;
        self.live.write().await.insert(
            id,
            Live {
                session: session.clone(),
                cancel,
                busy: false,
                prompt_handle: None,
                generation: 0,
            },
        );
        Ok(session)
    }

    /// Session courante (mémoire puis disque — rechargement après redémarrage).
    pub async fn get(&self, id: &str) -> Option<Session> {
        let live = self.live.read().await;
        match live.get(id) {
            Some(entry) => Some(entry.session.clone()),
            None => self.read(id).await,
        }
    }

    /// Liste le dépôt (filtre `cwd` si fourni), triée par `updated_at` décroissant.
    /// Ignore les `.snap.json` (snapshots) et les `.tmp` orphelins.
    pub async fn list(&self, cwd: Option<&Path>) -> Vec<Session> {
        let mut out = Vec::new();
        let Ok(mut entries) = fs::read_dir(&self.dir).await else {
            return out;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") || name.contains(".snap.") {
                continue;
            }
            if let Ok(raw) = fs::read_to_string(&path).await {
                if let Ok(s) = serde_json::from_str::<Session>(&raw) {
                    if cwd.is_none_or(|c| s.cwd == c) {
                        out.push(s);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Supprime la session (mémoire + fichier + snapshots associés).
    pub async fn delete(&self, id: &str) -> bool {
        self.live.write().await.remove(id);
        let main = fs::remove_file(self.path(id)).await.is_ok();
        for n in self.list_snapshots(id).await {
            let _ = fs::remove_file(self.snapshot_path(id, n)).await;
        }
        main
    }
}
