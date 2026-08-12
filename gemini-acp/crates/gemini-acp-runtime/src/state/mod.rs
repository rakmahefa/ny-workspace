//! État & persistance des sessions : mémoire (jetons d'annulation + verrou busy)
//! + dépôt disque `<data_dir>/sessions/<session_id>.json`.
//!
//! Architecture modulaire :
//! - [`types`] : types de données purs (Role, SessionMode, Session, TurnError, Live)
//! - [`persistence`] : I/O disque, CRUD, nettoyage au démarrage
//! - [`busy`] : sentinel inter-processus `.busy`
//! - [`snapshot`] : snapshots de session (création, prune, restauration)
//! - Ce fichier : struct `Store` + cycle de vie des tours + re-exports

mod busy;
mod persistence;
mod snapshot;
mod types;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::{watch, RwLock};

pub(crate) use types::MAX_SNAPSHOTS;
pub use types::{Live, Role, Session, SessionMode, TurnError};

/// Registre de sessions : mémoire (jetons + verrous) + dépôt à plat JSON par entrée.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    pub(crate) live: Arc<RwLock<HashMap<String, Live>>>,
}

impl Store {
    /// Démarre un tour : jeton d'annulation neuf + session (mémoire, sinon disque).
    /// Retourne `Err(TurnError::AlreadyRunning)` si un tour est déjà actif.
    pub async fn begin_turn(&self, id: &str) -> Result<(Session, watch::Receiver<bool>, u64), TurnError> {
        let mut live = self.live.write().await;
        if let Some(entry) = live.get_mut(id) {
            if entry.busy { return Err(TurnError::AlreadyRunning); }
            entry.busy = true;
            entry.generation += 1;
            let gen = entry.generation;
            let (tx, rx) = watch::channel(false);
            entry.cancel = tx;
            let _ = self.acquire_busy(id).await;
            return Ok((entry.session.clone(), rx, gen));
        }
        let session = self.read(id).await.ok_or_else(|| TurnError::NotFound(id.to_string()))?;
        let gen = 1u64;
        let (tx, rx) = watch::channel(false);
        live.insert(id.to_string(), Live { session: session.clone(), cancel: tx, busy: true, prompt_handle: None, generation: gen });
        let _ = self.acquire_busy(id).await;
        Ok((session, rx, gen))
    }

    /// Met à jour la session en mémoire + persistance sur disque, sans toucher au verrou `busy` ni au jeton d'annulation.
    pub async fn update_session<F>(&self, id: &str, f: F) -> Result<()>
    where F: FnOnce(&mut Session), {
        let mut live = self.live.write().await;
        if let Some(entry) = live.get_mut(id) {
            f(&mut entry.session);
            self.persist(&entry.session).await?;
            return Ok(());
        }
        let mut session = self.read(id).await.ok_or_else(|| anyhow::anyhow!("session introuvable: {id}"))?;
        f(&mut session);
        self.persist(&session).await?;
        Ok(())
    }

    /// Fin de tour : rafraîchit `updated_at`, persiste la session et libère `busy`.
    pub async fn end_turn(&self, id: &str, mut session: Session, expected_gen: u64) -> Result<()> {
        if expected_gen != 0 {
            let live = self.live.read().await;
            if let Some(entry) = live.get(id) {
                if entry.generation != expected_gen {
                    tracing::warn!(session = %id, expected_gen, current_gen = entry.generation, "end_turn: tour obsolète ignoré (état non persisté)");
                    bail!("tour obsolète: génération attendue {expected_gen}, courante {}", entry.generation);
                }
            }
        }
        session.updated_at = gemini_acp_config::core::time::now_iso();
        session.turn_count += 1;
        if let Some(current) = self.get(id).await {
            if !current.messages.is_empty() {
                let snap_n = current.messages.len();
                if let Ok(raw) = serde_json::to_string_pretty(&current) {
                    let _ = tokio::fs::write(self.snapshot_path(id, snap_n), &raw).await;
                }
                self.prune_snapshots(id, MAX_SNAPSHOTS).await;
            }
        }
        let persist_result = self.persist(&session).await;
        if let Some(entry) = self.live.write().await.get_mut(id) {
            entry.session = session.clone();
            entry.busy = false;
            entry.prompt_handle = None;
        }
        self.release_busy(id).await;
        persist_result
    }

    /// Annulation (`session/cancel`) : demande l'arrêt mais laisse le tour
    /// propriétaire du verrou jusqu'à `end_turn`. Le tour peut ainsi persister
    /// un état cohérent avant que la session redevienne disponible.
    pub async fn cancel(&self, id: &str) {
        let mut live = self.live.write().await;
        if let Some(entry) = live.get_mut(id) {
            let _ = entry.cancel.send(true);
        }
    }

    /// Annule tous les tours en cours. Les sentinelles `.busy` restent détenues
    /// par les tours jusqu'à leur sortie, afin d'éviter un chevauchement avec un
    /// nouveau prompt pendant que des tâches annulées sont encore vivantes.
    pub async fn cancel_all(&self) {
        let live = self.live.read().await;
        for entry in live.values() {
            let _ = entry.cancel.send(true);
        }
    }

    /// Ferme la session : annule le travail en cours, retire de la mémoire et conserve le fichier.
    pub async fn close(&self, id: &str) -> bool {
        let mut live = self.live.write().await;
        let existed = live.contains_key(id) || self.path(id).exists();
        if let Some(entry) = live.get(id) { let _ = entry.cancel.send(true); }
        live.remove(id);
        drop(live);
        self.release_busy(id).await;
        existed
    }

    /// Crée un fork d'une session existante.
    pub async fn fork(&self, source_id: &str) -> Result<Session> {
        let source = self.get(source_id).await.ok_or_else(|| anyhow::anyhow!("session source introuvable: {source_id}"))?;
        let new_id = format!("sess_{}", uuid::Uuid::new_v4().simple());
        let forked = source.fork(new_id);
        self.persist(&forked).await?;
        let (cancel, _) = watch::channel(false);
        self.live.write().await.insert(forked.id.clone(), Live { session: forked.clone(), cancel, busy: false, prompt_handle: None, generation: 0 });
        Ok(forked)
    }

    /// Attend que le prompt en cours soit terminé.
    pub async fn wait_prompt_done(&self, id: &str) {
        let done = {
            let mut live = self.live.write().await;
            live.get_mut(id).and_then(|e| e.prompt_handle.take())
        };
        if let Some(done) = done { let _ = done.await; }
    }

    /// Enregistre le signal de fin du prompt en cours dans l'entrée Live.
    pub async fn set_prompt_handle(&self, id: &str, done: tokio::sync::oneshot::Receiver<()>) {
        if let Some(entry) = self.live.write().await.get_mut(id) { entry.prompt_handle = Some(done); }
    }
}

#[cfg(test)]
#[path = "../test/state.rs"]
mod tests;
