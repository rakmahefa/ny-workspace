//! Messages d'erreur actionnables pour l'utilisateur (refactor M6 §5.5).
//!
//! Responsabilité unique : formater les erreurs Gemini en messages
//! compréhensibles et actionnables (cookies expirés, modèle inconnu, refus
//! de contenu, etc.).

/// Message d'erreur actionnable selon le contexte (refactor M6 §5.5).
/// Détecte `BardErrorInfo` pour cookies expirés, `SafetyBlocked` pour refus
/// de contenu, modèle inconnu, etc.
pub fn actionable_error_message(e: &anyhow::Error) -> String {
    let msg = format!("{e:#}");
    if msg.contains("BardErrorInfo") {
        format!(
            "Cookies expirés ou invalides ({msg}).\nRéexportez vendor/cookie.json depuis votre navigateur connecté à gemini.google.com, puis relancez l'agent."
        )
    } else if msg.contains("SafetyBlocked") || msg.contains("refus") || msg.contains("politique de sécurité") || msg.contains("politique de contenu") {
        // Extraire le message lisible après "SafetyBlocked" si présent.
        let user_msg = if let Some(pos) = msg.find("SafetyBlocked: ") {
            &msg[pos + "SafetyBlocked: ".len()..]
        } else if let Some(pos) = msg.find("n'a produit aucune réponse") {
            &msg[pos..]
        } else {
            "Gemini a refusé de répondre à ce prompt (politique de contenu)."
        };
        format!("Refus de contenu Gemini : {user_msg}")
    } else if msg.contains("Unknown model") || msg.contains("modèle inconnu") {
        format!("{msg}\nChoisissez un modèle valide dans la liste des config options.")
    } else {
        format!("Erreur Gemini: {msg}")
    }
}

/// Variante pour erreurs de streaming (chaîne déjà typée).
pub fn actionable_stream_error(e: &str) -> String {
    if e.contains("BardErrorInfo") {
        format!("Cookies expirés ou invalides ({}). Réexportez vendor/cookie.json.", e)
    } else if e.contains("SafetyBlocked") || e.contains("refus silencieux") || e.contains("politique de sécurité") || e.contains("politique de contenu") {
        let user_msg = if let Some(pos) = e.find("SafetyBlocked: ") {
            &e[pos + "SafetyBlocked: ".len()..]
        } else {
            "Gemini a refusé de répondre à ce prompt."
        };
        format!("Refus de contenu Gemini : {}", user_msg)
    } else if e.contains("content changed") {
        "Flux Gemini instable (divergence). Réessayez dans un instant.".to_string()
    } else {
        format!("Erreur Gemini: {}", e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actionable_error_message_cookies_expired() {
        let e = anyhow::anyhow!("Gemini upstream rejected request: BardErrorInfo [401]");
        let msg = actionable_error_message(&e);
        assert!(msg.contains("Cookies expirés"));
        assert!(msg.contains("cookie.json"));
    }

    #[test]
    fn actionable_error_message_unknown_model() {
        let e = anyhow::anyhow!("Unknown model: gpt-4o");
        let msg = actionable_error_message(&e);
        assert!(msg.contains("modèle"));
    }

    #[test]
    fn actionable_error_message_generique() {
        let e = anyhow::anyhow!("network timeout");
        let msg = actionable_error_message(&e);
        assert!(msg.contains("Erreur Gemini"));
    }

    #[test]
    fn actionable_error_message_safety_blocked() {
        let e = anyhow::anyhow!("SafetyBlocked: Gemini a refusé de répondre (blockReason: SAFETY).");
        let msg = actionable_error_message(&e);
        assert!(msg.contains("Refus de contenu Gemini"));
        assert!(msg.contains("blockReason: SAFETY"));
    }

    #[test]
    fn actionable_stream_error_safety_blocked() {
        let msg = actionable_stream_error("SafetyBlocked: Gemini n'a produit aucune réponse");
        assert!(msg.contains("Refus de contenu Gemini"));
    }

    #[test]
    fn actionable_stream_error_silent_refusal() {
        let msg = actionable_stream_error("refus silencieux");
        assert!(msg.contains("Refus de contenu Gemini"));
    }
}
