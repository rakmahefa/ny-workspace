//! Parsing des appels d'outils depuis la réponse texte Gemini.
//!
//! La Gemini web API ne supporte pas le function calling structuré.
//! On utilise la même approche que `gemini-web2api` : le modèle est
//! instruit de produire des blocs :
//!
//! ```tool_call
//! {"name": "func_name", "arguments": {...}}
//! ```
//!
//! Ce module les extrait par regex et retourne le texte nettoyé + la liste
//! des appels détectés.

use serde_json::{json, Value};

/// Un appel d'outil parsé depuis la réponse du modèle.
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    /// Nom de l'outil appelé.
    pub name: String,
    /// Arguments JSON (peut être un objet vide si absents).
    pub arguments: Value,
}

impl ParsedToolCall {
    /// Formate en bloc `tool_call` pour l'historique.
    pub fn to_history_block(&self) -> String {
        format!(
            "```tool_call\n{}\n```",
            json!({"name": self.name, "arguments": self.arguments})
        )
    }
}

/// Extrait les blocs ` ```tool_call ` de la réponse.
/// Retourne (texte nettoyé, liste des appels détectés).
///
/// Port adapté de `gemini-web2api::convert::common::parse_tool_calls`,
/// avec en plus le format `function_call` (compatibilité Google natif).
pub fn parse_tool_calls(text: &str) -> (String, Vec<ParsedToolCall>) {
    // Q1 : les regex sont statiques — on les compile une seule fois via
    // `OnceLock` plutôt qu'à chaque appel (la fonction est appelée à chaque
    // tour Gemini, parfois plusieurs fois dans la boucle outil).
    static RE_TOOL_CALL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_FUNC_CALL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_tool_call = RE_TOOL_CALL.get_or_init(|| {
        regex::Regex::new(r"(?s)```tool_call\s*\n(.*?)\n```").expect("regex statique")
    });
    let re_func_call = RE_FUNC_CALL.get_or_init(|| {
        regex::Regex::new(r"(?s)```function_call\s*\n(.*?)\n```").expect("regex statique")
    });

    let mut calls = Vec::new();
    let mut clean = text.to_string();

    // Format 1 : bloc code fence tool_call (format OpenAI)
    for cap in re_tool_call.captures_iter(&clean) {
        if let Some(call) = parse_single(cap[1].trim()) {
            calls.push(call);
        }
    }
    clean = re_tool_call.replace_all(&clean, "").trim().to_string();

    // Format 2 : ```function_call\n{...}\n``` (format Google)
    for cap in re_func_call.captures_iter(&clean) {
        if let Some(call) = parse_single_func(cap[1].trim()) {
            calls.push(call);
        }
    }
    clean = re_func_call.replace_all(&clean, "").trim().to_string();

    // Format 3 : JSON brut isolé `{"name": ..., "arguments"/"args": ...}`
    if calls.is_empty() && clean.trim_start().starts_with('{') {
        if let Ok(data) = serde_json::from_str::<Value>(clean.trim()) {
            if let Some(name) = data.get("name").and_then(Value::as_str) {
                let args = data
                    .get("arguments")
                    .or_else(|| data.get("args"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                calls.push(ParsedToolCall {
                    name: name.to_string(),
                    arguments: args,
                });
                clean.clear();
            }
        }
    }

    (clean, calls)
}

/// Parse un bloc JSON `tool_call` (format `{"name": ..., "arguments": ...}`).
fn parse_single(raw: &str) -> Option<ParsedToolCall> {
    let data: Value = serde_json::from_str(raw).ok()?;
    let name = data.get("name").and_then(Value::as_str)?.to_string();
    let arguments = data.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Some(ParsedToolCall { name, arguments })
}

/// Parse un bloc JSON `function_call` (format `{"name": ..., "args": ...}`).
fn parse_single_func(raw: &str) -> Option<ParsedToolCall> {
    let data: Value = serde_json::from_str(raw).ok()?;
    let name = data.get("name").and_then(Value::as_str)?.to_string();
    let arguments = data
        .get("args")
        .or_else(|| data.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some(ParsedToolCall { name, arguments })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_call_bloc_standard() {
        let text = "Voici le résultat:\n```tool_call\n{\"name\": \"file_read\", \"arguments\": {\"path\": \"/etc/hosts\"}}\n```\nEt voilà.";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[0].arguments["path"], "/etc/hosts");
        assert!(clean.contains("Voici le résultat"));
        assert!(clean.contains("Et voilà"));
        assert!(!clean.contains("tool_call"));
    }

    #[test]
    fn parse_function_call_bloc_google() {
        let text =
            "```function_call\n{\"name\": \"search\", \"args\": {\"pattern\": \"TODO\"}}\n```";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search");
        assert!(clean.trim().is_empty());
    }

    #[test]
    fn parse_multiple_tool_calls() {
        let text = "```tool_call\n{\"name\": \"file_read\", \"arguments\": {\"path\": \"a.rs\"}}\n```\n```tool_call\n{\"name\": \"file_read\", \"arguments\": {\"path\": \"b.rs\"}}\n```";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["path"], "a.rs");
        assert_eq!(calls[1].arguments["path"], "b.rs");
    }

    #[test]
    fn parse_json_brut_isole() {
        let text = "{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert!(clean.is_empty());
    }

    #[test]
    fn parse_pas_d_appel_texte_seul() {
        let text = "C'est une réponse simple sans aucun appel d'outil.";
        let (clean, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(clean, text);
    }

    #[test]
    fn parse_bloc_invalide_ignoré() {
        let text = "```tool_call\nceci n'est pas du json\n```";
        let (clean, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
        assert!(clean.trim().is_empty());
    }

    #[test]
    fn to_history_block_fmt() {
        let call = ParsedToolCall {
            name: "file_read".into(),
            arguments: json!({"path": "/tmp/f"}),
        };
        let block = call.to_history_block();
        assert!(block.contains("```tool_call"));
        assert!(block.contains("file_read"));
        assert!(block.contains("/tmp/f"));
    }
}
