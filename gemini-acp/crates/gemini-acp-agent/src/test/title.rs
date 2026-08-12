use super::*;

#[test]
fn derive_title_court_reste_telquel() {
    assert_eq!(derive_title("Bonjour"), "Bonjour");
    assert_eq!(derive_title("Refactor la fonction main"), "Refactor la fonction main");
}

#[test]
fn derive_title_long_est_tronque() {
    let title = derive_title("Ceci est un message utilisateur tres long qui depasse la limite de cinquante caracteres et doit etre tronque proprement");
    assert!(title.ends_with('…'));
    assert!(title.chars().count() <= MAX_TITLE_CHARS + 1);
}

#[test]
fn derive_title_multiligne_prend_premiere_ligne() {
    assert_eq!(derive_title("Première ligne\nDeuxième ligne"), "Première ligne");
}

#[test]
fn derive_title_vide_renvoie_defaut() {
    assert_eq!(derive_title(""), "Nouvelle session");
    assert_eq!(derive_title("   \n   "), "Nouvelle session");
}

#[test]
fn derive_title_unicode_compte_chars_pas_octets() {
    let title = derive_title(&"🚀".repeat(60));
    assert!(title.chars().count() <= MAX_TITLE_CHARS + 1);
}
