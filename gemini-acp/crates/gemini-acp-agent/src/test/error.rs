use super::*;

#[test]
fn actionable_error_message_cookies_expired() {
    let e = anyhow::anyhow!("Gemini upstream rejected request: BardErrorInfo [401]");
    let msg = actionable_error_message(&e);
    assert!(msg.contains("Authentification Gemini expirée"));
    assert!(msg.contains("cookie.json"));
}

#[test]
fn actionable_error_message_unknown_model() {
    let e = anyhow::anyhow!("Unknown model: gpt-4o");
    let msg = actionable_error_message(&e);
    assert!(msg.contains("modèle Gemini"));
    assert!(msg.contains("config options"));
}

#[test]
fn actionable_error_message_transport() {
    let e = anyhow::anyhow!("network timeout");
    let msg = actionable_error_message(&e);
    assert!(msg.contains("connexion avec Gemini"));
    assert!(msg.contains("réessayez"));
}

#[test]
fn actionable_error_message_context_overflow() {
    let e = anyhow::anyhow!("context window exceeded: maximum context length");
    let msg = actionable_error_message(&e);
    assert!(msg.contains("contexte"));
    assert!(msg.contains("nouvelle session"));
}

#[test]
fn actionable_error_message_safety_blocked() {
    let e = anyhow::anyhow!("SafetyBlocked: Gemini a refusé de répondre (blockReason: SAFETY).");
    let msg = actionable_error_message(&e);
    assert!(msg.contains("Gemini a refusé ce contenu"));
    assert!(msg.contains("blockReason: SAFETY"));
}

#[test]
fn actionable_stream_error_safety_blocked() {
    assert!(actionable_stream_error("SafetyBlocked: Gemini n'a produit aucune réponse").contains("Gemini a refusé ce contenu"));
}

#[test]
fn actionable_stream_error_silent_refusal() {
    assert!(actionable_stream_error("refus silencieux").contains("Gemini a refusé ce contenu"));
}

#[test]
fn actionable_stream_error_divergence_is_explicit() {
    let msg = actionable_stream_error("content changed during streaming");
    assert!(msg.contains("divergé"));
    assert!(msg.contains("réessayez"));
}
