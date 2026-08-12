//! User-facing, actionable error mapping.
//!
//! The important UX rule here is to distinguish *what the user can do next*
//! from the raw upstream failure. Claude ACP consistently surfaces the former
//! instead of turning every failure into an opaque "internal error" card.

fn normalize(raw: &str) -> String {
    raw.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_safety(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("safetyblocked")
        || lower.contains("refus")
        || lower.contains("politique de sécurité")
        || lower.contains("politique de contenu")
        || lower.contains("content policy")
}

fn is_context_overflow(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("context window")
        || lower.contains("context length")
        || lower.contains("maximum context")
        || lower.contains("too many tokens")
        || lower.contains("context overflow")
}

fn is_transport(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("network")
        || lower.contains("transport")
}

pub fn actionable_error_message(e: &anyhow::Error) -> String {
    let msg = normalize(&format!("{e:#}"));

    if msg.contains("BardErrorInfo") {
        return format!(
            "Authentification Gemini expirée. Réexportez `vendor/cookie.json` depuis un navigateur connecté à gemini.google.com, puis relancez l’agent.\n\nDétail : {msg}"
        );
    }

    if is_safety(&msg) {
        let detail = msg
            .strip_prefix("SafetyBlocked:")
            .or_else(|| msg.strip_prefix("SafetyBlocked"))
            .unwrap_or(&msg)
            .trim();
        return if detail.is_empty() {
            "Gemini a refusé ce contenu pour des raisons de politique. Reformulez le prompt ou retirez les éléments sensibles.".to_owned()
        } else {
            format!(
                "Gemini a refusé ce contenu pour des raisons de politique : {detail}\n\nAction : reformulez le prompt ou retirez les éléments sensibles."
            )
        };
    }

    if msg.contains("Unknown model") || msg.contains("modèle inconnu") {
        return format!(
            "Le modèle Gemini demandé n’est pas disponible. Choisissez un modèle valide dans les config options.\n\nDétail : {msg}"
        );
    }

    if is_context_overflow(&msg) {
        return format!(
            "Le contexte de cette session est trop volumineux. Réessayez après compaction, ou démarrez une nouvelle session.\n\nDétail : {msg}"
        );
    }

    if is_transport(&msg) {
        return format!(
            "La connexion avec Gemini a échoué ou a expiré. Vérifiez le réseau puis réessayez.\n\nDétail : {msg}"
        );
    }

    format!("Gemini n’a pas pu terminer cette opération.\n\nDétail : {msg}")
}

pub fn actionable_stream_error(raw: &str) -> String {
    let msg = normalize(raw);

    if msg.contains("BardErrorInfo") {
        return "Authentification Gemini expirée. Réexportez `vendor/cookie.json`, puis relancez l’agent.".to_owned();
    }

    if is_safety(&msg) {
        return "Gemini a refusé ce contenu pour des raisons de politique. Reformulez le prompt et réessayez.".to_owned();
    }

    if msg.contains("content changed") {
        return "Le flux Gemini a divergé pendant la génération. Aucun état partiel n’est considéré comme fiable : réessayez le tour.".to_owned();
    }

    if is_context_overflow(&msg) {
        return "La fenêtre de contexte est saturée. Réessayez après compaction ou démarrez une nouvelle session.".to_owned();
    }

    if is_transport(&msg) {
        return "La connexion avec Gemini a expiré ou a été interrompue. Vérifiez le réseau et réessayez.".to_owned();
    }

    format!("Le flux Gemini a échoué. Réessayez le tour.\n\nDétail : {msg}")
}

#[cfg(test)]
#[path = "../test/error.rs"]
mod tests;
