use super::*;
use std::path::Path;

#[tokio::test]
async fn cycle_create_persist_reload() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec!["/other".into()], "gemini-3.6-flash").await.unwrap();
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
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let (_, mut rx, _) = store.begin_turn(&s.id).await.unwrap();
    assert!(!*rx.borrow());
    store.cancel(&s.id).await;
    assert!(tokio::time::timeout(std::time::Duration::from_millis(500), rx.changed()).await.is_ok());
    store.end_turn(&s.id, store.get(&s.id).await.unwrap(), 1).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn tour_concurrent_renvoie_erreur() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let (_, _, gen) = store.begin_turn(&s.id).await.unwrap();
    assert!(matches!(store.begin_turn(&s.id).await, Err(TurnError::AlreadyRunning)));
    store.end_turn(&s.id, store.get(&s.id).await.unwrap(), gen).await.unwrap();
    assert!(store.begin_turn(&s.id).await.is_ok());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cancel_ne_libere_pas_le_verrou_avant_fin_du_tour() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let (session, _, gen) = store.begin_turn(&s.id).await.unwrap();
    store.cancel(&s.id).await;
    assert!(matches!(store.begin_turn(&s.id).await, Err(TurnError::AlreadyRunning)));
    store.end_turn(&s.id, session, gen).await.unwrap();
    assert!(store.begin_turn(&s.id).await.is_ok());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cleanup_tmp_orphelins_au_demarrage() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::write(dir.join("sessions").join("orphelin.json.tmp"), r#"{"incomplete": true}"#).unwrap();
    let _store = Store::open(&dir).await.unwrap();
    assert!(!dir.join("sessions").join("orphelin.json.tmp").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cancel_all_declenche_tous_les_jetons_sans_liberer_busy() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s1 = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let s2 = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let (session1, mut rx1, gen1) = store.begin_turn(&s1.id).await.unwrap();
    let (session2, mut rx2, gen2) = store.begin_turn(&s2.id).await.unwrap();
    store.cancel_all().await;
    assert!(tokio::time::timeout(std::time::Duration::from_millis(500), rx1.changed()).await.is_ok());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(500), rx2.changed()).await.is_ok());
    assert!(matches!(store.begin_turn(&s1.id).await, Err(TurnError::AlreadyRunning)));
    assert!(matches!(store.begin_turn(&s2.id).await, Err(TurnError::AlreadyRunning)));
    store.end_turn(&s1.id, session1, gen1).await.unwrap();
    store.end_turn(&s2.id, session2, gen2).await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn snapshot_cree_avant_chaque_tour() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
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
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn restore_snapshot_remplace_la_session() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let mut sess = store.get(&s.id).await.unwrap();
    sess.messages.push((Role::User, "Q1".into()));
    sess.messages.push((Role::Assistant, "R1".into()));
    store.end_turn(&s.id, sess, 0).await.unwrap();
    let mut sess = store.get(&s.id).await.unwrap();
    sess.messages.push((Role::User, "Q2".into()));
    sess.messages.push((Role::Assistant, "R2".into()));
    store.end_turn(&s.id, sess, 0).await.unwrap();
    store.restore_snapshot(&s.id, 2).await.unwrap();
    let restored = store.get(&s.id).await.unwrap();
    assert_eq!(restored.messages.len(), 2);
    assert_eq!(restored.messages[0].1, "Q1");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn prune_snapshots_garde_10_derniers() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    for i in 0..12 {
        let mut sess = store.get(&s.id).await.unwrap();
        sess.messages.push((Role::User, format!("Q{i}")));
        sess.messages.push((Role::Assistant, format!("R{i}")));
        store.end_turn(&s.id, sess, 0).await.unwrap();
    }
    let snaps = store.list_snapshots(&s.id).await;
    assert!(snaps.len() <= MAX_SNAPSHOTS);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn list_ignore_les_snapshots() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let mut sess = store.get(&s.id).await.unwrap();
    sess.messages.push((Role::User, "Q1".into()));
    sess.messages.push((Role::Assistant, "R1".into()));
    store.end_turn(&s.id, sess, 0).await.unwrap();
    assert_eq!(store.list(None).await.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn set_prompt_handle_puis_wait_resout_a_la_fin() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    store.set_prompt_handle(&s.id, done_rx).await;
    let store2 = store.clone();
    let sid = s.id.clone();
    let mut task = tokio::spawn(async move { store2.wait_prompt_done(&sid).await; store2.begin_turn(&sid).await.map(|_| ()) });
    assert!(tokio::time::timeout(std::time::Duration::from_millis(150), &mut task).await.is_err());
    let _ = done_tx.send(());
    let resumed = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.expect("la tâche doit reprendre").expect("begin_turn doit réussir");
    assert!(resumed.is_ok());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn wait_prompt_done_sans_handle_ne_bloque_pas() {
    let dir = std::env::temp_dir().join(format!("acp-test-{}", uuid::Uuid::new_v4().simple()));
    let store = Store::open(&dir).await.unwrap();
    let s = store.create("/tmp".into(), vec![], "gemini-3.6-flash").await.unwrap();
    let ok = tokio::time::timeout(std::time::Duration::from_millis(200), async { store.wait_prompt_done(&s.id).await }).await;
    assert!(ok.is_ok(), "wait_prompt_done sans handle ne doit pas bloquer");
    std::fs::remove_dir_all(&dir).ok();
}
