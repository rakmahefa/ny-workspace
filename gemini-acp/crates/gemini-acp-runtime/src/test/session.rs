use super::*;

#[test]
fn valide_id_session() {
    assert!(SessionManager::validate_id("sess_0123456789abcdef0123456789abcdef").is_ok());
    assert!(SessionManager::validate_id("sess_0123456789ABCDEF0123456789abcdef").is_err());
    assert!(SessionManager::validate_id("../sess_0123456789abcdef0123456789abcdef").is_err());
}

#[test]
fn sanitize_title_collabse_et_tronque() {
    assert_eq!(SessionManager::sanitize_title("  hello\n   world  ").as_deref(), Some("hello world"));
    let long = "a".repeat(MAX_TITLE_LENGTH + 40);
    let title = SessionManager::sanitize_title(&long).unwrap();
    assert_eq!(title.chars().count(), MAX_TITLE_LENGTH);
    assert!(title.ends_with('…'));
}
