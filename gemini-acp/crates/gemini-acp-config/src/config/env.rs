//! Helpers d'environnement (refactor M9 §6.1).
//!
//! Centralise la lecture des variables d'environnement et la résolution du
//! dépôt de sessions. Était inline dans `main.rs` avant le refactor.

use std::path::PathBuf;

/// Lit une variable d'environnement, retourne `default` si absente.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Dépôt par défaut : `$GEMINI_ACP_DATA_DIR`, sinon `$XDG_DATA_HOME/gemini-acp`,
/// puis `~/.local/share/gemini-acp`.
pub fn data_dir_default() -> PathBuf {
    if let Ok(d) = std::env::var("GEMINI_ACP_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(x).join("gemini-acp");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/gemini-acp");
    }
    PathBuf::from(".")
}

/// Compte Google (`/u/<n>`) — absent = compte par défaut.
pub fn parse_auth_user() -> Option<u32> {
    std::env::var("GEMINI_ACP_AUTH_USER")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_retourne_default_si_absent() {
        let key = "GEMINI_ACP_TEST_ENV_OR_NON_DEFINI";
        assert_eq!(env_or(key, "défaut"), "défaut");
    }

    #[test]
    fn data_dir_default_avec_env_var() {
        // Q14 : test réellement la variable d'environnement cette fois.
        // On sauvegarde la valeur précédente pour ne pas polluer les autres
        // tests, puis on définit GEMINI_ACP_DATA_DIR, on appelle la fonction,
        // et on restaure.
        let prev = std::env::var("GEMINI_ACP_DATA_DIR").ok();
        std::env::set_var("GEMINI_ACP_DATA_DIR", "/tmp/test-data-dir-q14");
        let dir = data_dir_default();
        assert_eq!(dir, PathBuf::from("/tmp/test-data-dir-q14"));
        // Restaure l'état précédent.
        match prev {
            Some(v) => std::env::set_var("GEMINI_ACP_DATA_DIR", v),
            None => std::env::remove_var("GEMINI_ACP_DATA_DIR"),
        }
    }
}
