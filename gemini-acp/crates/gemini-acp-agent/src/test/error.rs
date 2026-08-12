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
    assert!(actionable_error_message(&e).contains("modèle"));
}

#[test]
fn actionable_error_message_generique() {
    let e = anyhow::anyhow!("network timeout");
    assert!(actionable_error_message(&e).contains("Erreur Gemini"));
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
    assert!(actionable_stream_error("SafetyBlocked: Gemini n'a produit aucune réponse").contains("Refus de contenu Gemini"));
}

#[test]
fn actionable_stream_error_silent_refusal() {
    assert!(actionable_stream_error("refus silencieux").contains("Refus de contenu Gemini"));
}
