use super::*;

#[test]
fn usage_estime_tokens_en_contexte() {
    let u = usage_update("question 🚀", "réponse");
    assert_eq!(u.used, (10 + 8) / 4);
    assert_eq!(u.size, CONTEXT_TOKENS);
    assert!(u.cost.is_none());
}
