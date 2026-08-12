//! Messages d'erreur actionnables pour l'utilisateur.

pub fn actionable_error_message(e: &anyhow::Error) -> String {
    let msg = format!("{e:#}");
    if msg.contains("BardErrorInfo") {
        format!("Cookies expirés ou invalides ({msg}).\nRéexportez vendor/cookie.json depuis votre navigateur connecté à gemini.google.com, puis relancez l'agent.")
    } else if msg.contains("SafetyBlocked") || msg.contains("refus") || msg.contains("politique de sécurité") || msg.contains("politique de contenu") {
        let user_msg = if let Some(pos) = msg.find("SafetyBlocked: ") { &msg[pos + "SafetyBlocked: ".len()..] } else if let Some(pos) = msg.find("n'a produit aucune réponse") { &msg[pos..] } else { "Gemini a refusé de répondre à ce prompt (politique de contenu)." };
        format!("Refus de contenu Gemini : {user_msg}")
    } else if msg.contains("Unknown model") || msg.contains("modèle inconnu") {
        format!("{msg}\nChoisissez un modèle valide dans la liste des config options.")
    } else { format!("Erreur Gemini: {msg}") }
}

pub fn actionable_stream_error(e: &str) -> String {
    if e.contains("BardErrorInfo") {
        format!("Cookies expirés ou invalides ({}). Réexportez vendor/cookie.json.", e)
    } else if e.contains("SafetyBlocked") || e.contains("refus silencieux") || e.contains("politique de sécurité") || e.contains("politique de contenu") {
        let user_msg = if let Some(pos) = e.find("SafetyBlocked: ") { &e[pos + "SafetyBlocked: ".len()..] } else { "Gemini a refusé de répondre à ce prompt." };
        format!("Refus de contenu Gemini : {}", user_msg)
    } else if e.contains("content changed") { "Flux Gemini instable (divergence). Réessayez dans un instant.".to_string() }
    else { format!("Erreur Gemini: {}", e) }
}

#[cfg(test)]
#[path = "../test/error.rs"]
mod tests;
