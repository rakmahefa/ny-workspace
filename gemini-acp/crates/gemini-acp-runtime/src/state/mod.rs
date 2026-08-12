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
    pub async fn begin_turn(
        &self,
        id: &str,
    ) -> Result<(Session, watch::Receiver<bool>, u64), TurnError> {
        let mut live = self.live.write().await;
        if let Some(entry) = live.get_mut(id) {
            if entry.busy {
                return Err(TurnError::AlreadyRunning);
            }
            entry.busy = true;
            entry.generation += 1;
            let gen = entry.generation;
            let (tx, rx) = watch::channel(false);
            entry.cancel = tx;
            let _ = self.acquire_busy(id).await;
            return Ok((entry.session.clone(), rx, gen));
        }
        // Pas en mémoire : charger depuis le disque.
        let session = self
            .read(id)
            .await
            .ok_or_else(|| TurnError::NotFound(id.to_string()))?;
        let gen = 1u64;
        let (tx, rx) = watch::channel(false);
        live.insert(
            id.to_string(),
            Live {
                session: session.clone(),
                cancel: tx,
                busy: true,
                prompt_handle: None,
                generation: gen,
            },
        );
        let _ = self.acquire_busy(id).await;
        Ok((session, rx, gen))
    }

    /// Met à jour la session en mémoire + persistance sur disque, **sans** toucher
    /// au verrou `busy` ni au jeton d'annulation.
    pub async fn update_session<F>(&self, id: &str, f: F) -> Result<()>
    where
        F: FnOnce(&mut Session),
    {
        let mut live = self.live.write().await;
        if let Some(entry) = live.get_mut(id) {
            f(&mut entry.session);
            self.persist(&entry.session).await?;
            return Ok(());
        }
        let mut session = self
            .read(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("session introuvable: {id}"))?;
        f(&mut session);
        self.persist(&session).await?;
        Ok(())
    }

    /// Fin de tour : `updated_at` rafraîchi, entrée mémoire + fichier réécrits.
    /// Libère toujours le verrou `busy`, même en cas d'erreur de persistance.
    ///
    /// Écrit un snapshot `<id>.<n>.snap.json` AVANT la persistance, puis prune
    /// les snapshots pour ne garder que les 10 derniers.
    /// Incrémente `turn_count` et réinitialise `prompt_handle`.
    pub async fn end_turn(&self, id: &str, mut session: Session, expected_gen: u64) -> Result<()> {
        // Garde-fou anti-course : un tour devenu obsolète (cancel puis nouveau
        // tour entre-temps) ne doit pas écraser l'état d'un tour plus récent.
        // `expected_gen == 0` = pas de vérification (chemin d'erreur/restore).
        if expected_gen != 0 {
            let live = self.live.read().await;
            if let Some(entry) = live.get(id) {
                if entry.generation != expected_gen {
                    tracing::warn!(
                        session = %id,
                        expected_gen = expected_gen,
                        current_gen = entry.generation,
                        "end_turn: tour obsolète ignoré (état non persisté)"
                    );
                    bail!(
                        "tour obsolète: generation attendue {expected_gen}, courante {}",
                        entry.generation
                    );
                }
            }
        }

        session.updated_at = gemini_acp_config::core::time::now_iso();
        session.turn_count += 1;

        // Snapshot de l'état actuel (avant modification) pour undo.
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

    /// Annulation (`session/cancel`/`session/close`) : pose le jeton du tour courant.
    /// Libère aussi le verrou `busy` pour permettre un nouveau tour après annulation.
    pub async fn cancel(&self, id: &str) {
        let needs_release = {
            let mut live = self.live.write().await;
            if let Some(entry) = live.get_mut(id) {
                let _ = entry.cancel.send(true);
                entry.busy = false;
                entry.generation += 1;
                true
            } else {
                false
            }
        };
        if needs_release {
            self.release_busy(id).await;
        }
    }

    /// Annule tous les tours en cours (shutdown gracieux SIGINT/SIGTERM).
    /// Libère aussi les verrous `busy` en mémoire et les sentinelles `.busy` sur disque.
    pub async fn cancel_all(&self) {
        let ids: Vec<String> = {
            let live = self.live.read().await;
            for entry in live.values() {
                let _ = entry.cancel.send(true);
            }
            live.keys().cloned().collect()
        };
        let mut live = self.live.write().await;
        for id in &ids {
            if let Some(entry) = live.get_mut(id) {
                entry.busy = false;
            }
        }
        drop(live);
        for id in &ids {
            self.release_busy(id).await;
        }
    }

    /// Ferme la session : annule le travail en cours, retire de la mémoire,
    /// conserve le fichier.
    pub async fn close(&self, id: &str) -> bool {
        let mut live = self.live.write().await;
        let existed = live.contains_key(id) || self.path(id).exists();
        if let Some(entry) = live.get(id) {
            let _ = entry.cancel.send(true);
        }
        live.remove(id);
        drop(live);
        self.release_busy(id).await;
        existed
    }

    /// Crée un fork d'une session existante.
    pub async fn fork(&self, source_id: &str) -> Result<Session> {
        let source = self
            .get(source_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("session source introuvable: {source_id}"))?;
        let new_id = format!("sess_{}", uuid::Uuid::new_v4().simple());
        let forked = source.fork(new_id);
        self.persist(&forked).await?;
        let (cancel, _) = watch::channel(false);
        self.live.write().await.insert(
            forked.id.clone(),
            Live {
                session: forked.clone(),
                cancel,
                busy: false,
                prompt_handle: None,
                generation: 0,
            },
        );
        Ok(forked)
    }

    /// Attend que le prompt en cours soit terminé.
    pub async fn wait_prompt_done(&self, id: &str) {
        let done = {
            let mut live = self.live.write().await;
            live.get_mut(id).and_then(|e| e.prompt_handle.take())
        };
        if let Some(done) = done {
            let _ = done.await;
        }
    }

    /// Enregistre le signal de fin du prompt en cours dans l'entrée Live.
    /// Le receiver est résolu quand la tâche du tour termine (serialisation).
    pub async fn set_prompt_handle(&self, id: &str, done: tokio::sync::oneshot::Receiver<()>) {
        if let Some(entry) = self.live.write().await.get_mut(id) {
            entry.prompt_handle = Some(done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn cycle_create_persist_reload() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec!["/other".into()], "gemini-3.6-flash")
            .await
            .unwrap();
        assert!(s.id.starts_with("sess_"));
        assert_eq!(s.messages.len(), 0);

        let mut s2 = store.get(&s.id).await.unwrap();
        s2.messages.push((Role::User, "bonjour".into()));
        s2.created_at = "2000-01-01T00:00:00Z".to_string();
        store.end_turn(&s.id, s2, 0).await.unwrap();

        let reloaded = store.get(&s.id).await.unwrap();
        assert_eq!(reloaded.messages, vec![(Role::User, "bonjour".into())]);
        assert_ne!(reloaded.updated_at, reloaded.created_at);

        assert_eq!(store.list(None).await.len(), 1);
        assert_eq!(store.list(Some(Path::new("/nope"))).await.len(), 0);
        assert!(store.delete(&s.id).await);
        assert!(store.get(&s.id).await.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn annulation_declenche_le_jeton() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();
        let (_, mut rx, _) = store.begin_turn(&s.id).await.unwrap();
        assert!(!*rx.borrow());
        store.cancel(&s.id).await;
        let cancelled = tokio::time::timeout(std::time::Duration::from_millis(500), rx.changed())
            .await
            .is_ok();
        assert!(cancelled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tour_concurrent_renvoie_erreur() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();
        let _ = store.begin_turn(&s.id).await.unwrap();
        let second = store.begin_turn(&s.id).await;
        assert!(matches!(second, Err(TurnError::AlreadyRunning)));
        store
            .end_turn(&s.id, store.get(&s.id).await.unwrap(), 0)
            .await
            .unwrap();
        let third = store.begin_turn(&s.id).await;
        assert!(third.is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cancel_libere_le_verrou_busy() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();
        let _ = store.begin_turn(&s.id).await.unwrap();
        store.cancel(&s.id).await;
        let second = store.begin_turn(&s.id).await;
        assert!(second.is_ok(), "cancel doit libérer le verrou busy");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cleanup_tmp_orphelins_au_demarrage() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::write(
            dir.join("sessions").join("orphelin.json.tmp"),
            r#"{"incomplete": true}"#,
        )
        .unwrap();
        let store = Store::open(&dir).await.unwrap();
        assert!(!dir.join("sessions").join("orphelin.json.tmp").exists());
        let _ = store;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cancel_all_annule_tous_les_tours() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s1 = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();
        let s2 = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();
        let (_, mut rx1, _) = store.begin_turn(&s1.id).await.unwrap();
        let (_, mut rx2, _) = store.begin_turn(&s2.id).await.unwrap();
        store.cancel_all().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(500), rx1.changed())
                .await
                .is_ok()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(500), rx2.changed())
                .await
                .is_ok()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn snapshot_cree_avant_chaque_tour() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();

        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, "Q1".into()));
        sess.messages.push((Role::Assistant, "R1".into()));
        store.end_turn(&s.id, sess, 0).await.unwrap();
        assert_eq!(store.list_snapshots(&s.id).await.len(), 0);

        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, "Q2".into()));
        sess.messages.push((Role::Assistant, "R2".into()));
        store.end_turn(&s.id, sess, 0).await.unwrap();
        assert_eq!(store.list_snapshots(&s.id).await, vec![2]);

        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, "Q3".into()));
        sess.messages.push((Role::Assistant, "R3".into()));
        store.end_turn(&s.id, sess, 0).await.unwrap();
        assert_eq!(store.list_snapshots(&s.id).await, vec![4, 2]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn restore_snapshot_remplace_la_session() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();

        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, "Q1".into()));
        sess.messages.push((Role::Assistant, "R1".into()));
        store.end_turn(&s.id, sess, 0).await.unwrap();

        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, "Q2".into()));
        sess.messages.push((Role::Assistant, "R2".into()));
        store.end_turn(&s.id, sess, 0).await.unwrap();

        let current = store.get(&s.id).await.unwrap();
        assert_eq!(current.messages.len(), 4);
        assert_eq!(store.list_snapshots(&s.id).await, vec![2]);

        store.restore_snapshot(&s.id, 2).await.unwrap();
        let restored = store.get(&s.id).await.unwrap();
        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].1, "Q1");
        assert_eq!(restored.messages[1].1, "R1");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn prune_snapshots_garde_10_derniers() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();

        for i in 0..12 {
            let mut sess = store.get(&s.id).await.unwrap();
            sess.messages.push((Role::User, format!("Q{i}")));
            sess.messages.push((Role::Assistant, format!("R{i}")));
            store.end_turn(&s.id, sess, 0).await.unwrap();
        }

        let snaps = store.list_snapshots(&s.id).await;
        assert!(
            snaps.len() <= MAX_SNAPSHOTS,
            "{} snapshots > {}",
            snaps.len(),
            MAX_SNAPSHOTS
        );
        assert_eq!(snaps[0], 22);
        assert_eq!(snaps[snaps.len() - 1], 22 - 2 * (snaps.len() - 1));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn list_ignore_les_snapshots() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();
        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, "Q1".into()));
        sess.messages.push((Role::Assistant, "R1".into()));
        store.end_turn(&s.id, sess, 0).await.unwrap();
        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, "Q2".into()));
        store.end_turn(&s.id, sess, 0).await.unwrap();
        assert_eq!(store.list(None).await.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- Chantier C — sérialisation des prompts (spec §4) ----

    #[tokio::test]
    async fn set_prompt_handle_puis_wait_resout_a_la_fin() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();

        // Le handle est posé par le handler AVANT le spawn du tour (spec C2).
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        store.set_prompt_handle(&s.id, done_rx).await;

        // La tâche du tour : attend le done AVANT begin_turn (le 2ᵉ prompt).
        let store2 = store.clone();
        let sid = s.id.clone();
        let mut task = tokio::spawn(async move {
            store2.wait_prompt_done(&sid).await;
            store2.begin_turn(&sid).await.map(|_| ())
        });

        // Tant que le sender n'est pas résolu, la tâche doit rester bloquée.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut task)
                .await
                .is_err(),
            "wait_prompt_done doit bloquer tant que le tour précédent n'est pas fini"
        );

        // Résolution du handle : la tâche reprend (le handle consommé résout au send).
        let _ = done_tx.send(());
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("la tâche doit reprendre après résolution du handle")
            .expect("begin_turn doit réussir après le wait");
        assert!(resumed.is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn wait_prompt_done_sans_handle_ne_bloque_pas() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();
        // Aucun handle posé : wait_prompt_done doit retourner immédiatement.
        let ok = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            store.wait_prompt_done(&s.id).await;
        })
        .await;
        assert!(
            ok.is_ok(),
            "wait_prompt_done sans handle ne doit pas bloquer"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- Chantier D — garde-fou end_turn / TurnGuard::drop (spec §5) ----

    #[tokio::test]
    async fn end_turn_obsolete_ne_persiste_pas() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();

        // Tour A : gen = 1.
        let (session_a, _, gen_a) = store.begin_turn(&s.id).await.unwrap();
        assert_eq!(gen_a, 1);

        // Copie modifiée en mémoire (le tour pense persister un message).
        let mut modified = session_a.clone();
        modified.messages.push((Role::User, "message perdu".into()));

        // Annulation → gen = 2 (le tour A devient obsolète).
        store.cancel(&s.id).await;

        // end_turn du tour obsolète (gen 1) : Err attendu, aucun effet de bord.
        let err = store.end_turn(&s.id, modified.clone(), gen_a).await;
        assert!(err.is_err(), "end_turn obsolète doit échouer");

        // Le fichier disque ne contient pas le message (aucune persistance).
        let on_disk = store.get(&s.id).await.unwrap();
        assert!(
            on_disk.messages.iter().all(|(_, t)| t != "message perdu"),
            "le message du tour obsolète ne doit pas être persisté"
        );
        // updated_at inchangé.
        assert_eq!(on_disk.updated_at, session_a.updated_at);

        store.cancel(&s.id).await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn end_turn_obsolete_ne_relache_pas_busy() {
        let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&dir).await.unwrap();
        let s = store
            .create("/tmp".into(), vec![], "gemini-3.6-flash")
            .await
            .unwrap();

        // Tour A (gen 1), copie conservée pour simuler le Drop obsolète.
        let (session_a, _, gen_a) = store.begin_turn(&s.id).await.unwrap();
        assert_eq!(gen_a, 1);

        // Cancel → gen 2, busy libéré.
        store.cancel(&s.id).await;

        // Tour B démarre : gen 3, busy = true.
        let (_session_b, _, gen_b) = store.begin_turn(&s.id).await.unwrap();
        assert_eq!(gen_b, 3);

        // Le Drop du tour A (gen 1) tente end_turn : Err — ne relâche pas busy.
        let err = store.end_turn(&s.id, session_a, gen_a).await;
        assert!(err.is_err(), "end_turn obsolète doit échouer");

        // Le verrou busy de B reste posé : un 3ᵉ begin_turn renvoie AlreadyRunning.
        let third = store.begin_turn(&s.id).await;
        assert!(
            matches!(third, Err(TurnError::AlreadyRunning)),
            "le busy du tour B doit rester posé après l'end_turn obsolète"
        );

        // Nettoyage : fin du tour B avec sa génération réelle.
        store
            .end_turn(&s.id, store.get(&s.id).await.unwrap(), gen_b)
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
