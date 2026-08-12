//! Helpers communs aux formats OpenAI / Codex / Google (refactor M9 §6.4).
//!
//! Régroupe : résolution stricte du modèle, estimation d'usage, bloc
//! `tool_call`, parsing des `tool_calls` en sortie, énum `ToolChoice` et
//! ses méthodes, avertissement `xsrf_token` ignoré.

use serde_json::{json, Value};
use tracing::warn;

/// Résout un nom de modèle comme `_resolve_model` du vendor : erreur 400 si le
/// nom (hors suffixe `@think=`) est inconnu — contrairement à `gemini_acp_config::core::models::resolve`
/// qui replie sur le défaut. Le suffixe invalide est aussi une erreur.
pub fn resolve_model_strict(
    requested: &str,
    default: &str,
) -> Result<gemini_acp_config::core::models::Resolved, String> {
    let base = requested.split("@think=").next().unwrap_or(requested);
    if !gemini_acp_config::core::models::MODEL_KEYS.contains(&base) {
        return Err(format!("Unknown model: {requested}"));
    }
    gemini_acp_config::core::models::resolve(requested, default)
}

/// Usage estimé (Q11 — cohérence avec `acp::prompt::notify`). On utilise
/// `chars().count() / 4` plutôt que `len() / 4` (octets) pour mieux
/// refléter les tokens sur du texte non-ASCII (CJK, emoji, accents).
pub fn usage(prompt: &str, completion: &str) -> Value {
    let pt = prompt.chars().count() / 4;
    let ct = completion.chars().count() / 4;
    json!({
        "prompt_tokens": pt,
        "completion_tokens": ct,
        "total_tokens": pt + ct,
    })
}

/// Bloc ` ```tool_call\n{"name": .., "arguments": ..}\n``` ` (syntaxe de sortie).
pub fn tool_call_block(name: &str, args: &Value) -> String {
    format!(
        "```tool_call\n{}\n```",
        json!({"name": name, "arguments": args})
    )
}

/// Extrait les blocs ` ```tool_call ` de la réponse. Retourne le texte nettoyé
/// et la liste des appels (`tool_calls` OpenAI). Port de `parse_tool_calls` :
/// un bloc non-JSON ou sans `name` est ignoré silencieusement.
pub fn parse_tool_calls(text: &str) -> (String, Vec<Value>) {
    // Q1 : la regex est statique, on la compile une seule fois via `OnceLock`.
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?s)```tool_call\s*\n(.*?)\n```").expect("regex statique")
    });
    let mut tool_calls = Vec::new();
    for cap in re.captures_iter(text) {
        let data: Value = match serde_json::from_str(cap[1].trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(name) = data.get("name").and_then(Value::as_str) else {
            continue;
        };
        let arguments = data.get("arguments").cloned().unwrap_or_else(|| json!({}));
        // Q12 : on utilise l'UUID complet (32 hex) plutôt que les 8 premiers
        // caractères, pour éviter toute collision sur un volume élevé
        // d'appels d'outils. Les 8 hex donnaient 32 bits → ~4 milliards de
        // possibilités, ce qui devient risqué à l'échelle.
        let call_id = uuid::Uuid::new_v4().simple().to_string();
        tool_calls.push(json!({
            "id": format!("call_{call_id}"),
            "type": "function",
            "function": {
                "name": name,
                "arguments": arguments.to_string(),
            },
        }));
    }
    let clean = re.replace_all(text, "").trim().to_string();
    (clean, tool_calls)
}

/// `tool_choice` OpenAI (chat/completions, responses) : `auto` (défaut),
/// `none` (pas d'appel d'outil), `required` (au moins un appel), ou objet
/// `{type: function, function: {name}}` (outil précis). Port de
/// `req.get("tool_choice", "auto")` + `_build_tool_choice_instruction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

impl ToolChoice {
    pub fn parse(v: Option<&Value>) -> Self {
        match v {
            Some(Value::String(s)) => match s.as_str() {
                "none" => Self::None,
                "required" => Self::Required,
                _ => Self::Auto,
            },
            Some(Value::Object(_)) => {
                let name = v
                    .and_then(|o| o.get("function"))
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if name.is_empty() {
                    Self::Auto
                } else {
                    Self::Named(name.to_string())
                }
            }
            _ => Self::Auto,
        }
    }

    /// `true` si l'appel d'outil est interdit (`none`) → pas de section tools,
    /// pas de parsing de `tool_call` en sortie, streaming delta pur.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Contrainte ajoutée à la fin de la section tools (port du vendor).
    pub fn instruction(&self) -> String {
        match self {
            Self::Auto | Self::None => String::new(),
            Self::Required => {
                "\n\nIMPORTANT: You MUST call at least one tool. Do not respond with text only."
                    .into()
            }
            Self::Named(name) => {
                format!(
                    "\n\nIMPORTANT: You MUST call the tool \"{name}\". Do not call other tools."
                )
            }
        }
    }
}

/// Avertit une fois que `xsrf_token` est défini mais ignoré (le client gemini
/// récupère `SNlM0e` automatiquement, spec §4.5).
pub fn warn_xsrf_ignored(xsrf: Option<&str>) {
    if xsrf.is_some() {
        warn!(
            "xsrf_token configuré mais ignoré : le client gemini récupère SNlM0e automatiquement"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_choice_parse_et_instruction() {
        assert_eq!(ToolChoice::parse(None), ToolChoice::Auto);
        assert_eq!(ToolChoice::parse(Some(&json!("none"))), ToolChoice::None);
        assert_eq!(
            ToolChoice::parse(Some(&json!("required"))),
            ToolChoice::Required
        );
        assert_eq!(ToolChoice::parse(Some(&json!("auto"))), ToolChoice::Auto);
        assert_eq!(
            ToolChoice::parse(Some(
                &json!({"type": "function", "function": {"name": "lire"}})
            )),
            ToolChoice::Named("lire".into())
        );
        assert!(ToolChoice::Auto.instruction().is_empty());
        assert!(ToolChoice::Required
            .instruction()
            .contains("at least one tool"));
        assert!(ToolChoice::Named("lire".into())
            .instruction()
            .contains("lire"));
        assert!(ToolChoice::None.is_none());
        assert!(!ToolChoice::Auto.is_none());
    }
}
