//! Dérivation automatique du titre de session (refactor M7 §3.4).
//!
//! Responsabilité unique : extraire un titre court et lisible du premier
//! message utilisateur. Tronque proprement sur limite de chars (pas d'octets)
//! avec ellipsis unicode.

/// Longueur maximale du titre auto-dérivé du premier message utilisateur.
pub const MAX_TITLE_CHARS: usize = 50;

/// Dérive un titre lisible du premier message utilisateur (refactor M7 §3.4).
/// Tronque proprement sur limite de chars (pas d'octets) avec ellipsis.
pub fn derive_title(first_user_message: &str) -> String {
    let trimmed = first_user_message.trim();
    let single_line = trimmed.split('\n').next().unwrap_or("").trim();
    if single_line.is_empty() {
        return "Nouvelle session".to_string();
    }
    let char_count = single_line.chars().count();
    if char_count <= MAX_TITLE_CHARS {
        return single_line.to_string();
    }
    // Troncature sur chars (sécurise multi-octets).
    let cutoff = single_line
        .char_indices()
        .nth(MAX_TITLE_CHARS - 1)
        .map(|(i, _)| i)
        .unwrap_or(single_line.len());
    format!("{}…", &single_line[..cutoff])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_title_court_reste_telquel() {
        assert_eq!(derive_title("Bonjour"), "Bonjour");
        assert_eq!(
            derive_title("Refactor la fonction main"),
            "Refactor la fonction main"
        );
    }

    #[test]
    fn derive_title_long_est_tronque() {
        let long = "Ceci est un message utilisateur tres long qui depasse la limite de cinquante caracteres et doit etre tronque proprement";
        let title = derive_title(long);
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= MAX_TITLE_CHARS + 1); // +1 pour l'ellipsis
    }

    #[test]
    fn derive_title_multiligne_prend_premiere_ligne() {
        assert_eq!(
            derive_title("Première ligne\nDeuxième ligne"),
            "Première ligne"
        );
    }

    #[test]
    fn derive_title_vide_renvoie_defaut() {
        assert_eq!(derive_title(""), "Nouvelle session");
        assert_eq!(derive_title("   \n   "), "Nouvelle session");
    }

    #[test]
    fn derive_title_unicode_compte_chars_pas_octets() {
        // 🚀 = 1 char, 4 octets. La limite est sur les chars.
        let title = derive_title(&"🚀".repeat(60));
        assert!(title.chars().count() <= MAX_TITLE_CHARS + 1);
    }
}
