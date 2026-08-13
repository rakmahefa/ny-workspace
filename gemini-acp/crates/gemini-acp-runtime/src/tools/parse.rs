//! Parsing normalisé des appels d'outils depuis les réponses texte Gemini.
//!
//! Le parser reste indépendant d'ACP : il donne à chaque appel une identité
//! stable et une catégorie sémantique afin que l'agent ne recrée pas l'identité
//! au moment de l'exécution.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedToolKind {
    Tool,
    Elicitation,
}

impl ParsedToolKind {
    pub fn is_elicitation(self) -> bool {
        matches!(self, Self::Elicitation)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub kind: ParsedToolKind,
}

impl ParsedToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        let name = name.into();
        let kind = classify_tool_kind(&name);
        Self { id: id.into(), name, arguments, kind }
    }

    pub fn is_elicitation(&self) -> bool {
        self.kind.is_elicitation()
    }

    pub fn to_history_block(&self) -> String {
        format!(
            "```tool_call\n{}\n```",
            json!({"id": self.id, "name": self.name, "arguments": self.arguments})
        )
    }
}

fn classify_tool_kind(name: &str) -> ParsedToolKind {
    let normalized = name
        .trim()
        .to_ascii_lowercase()
        .replace(['-', '_'], "");
    match normalized.as_str() {
        "askuserquestion" | "elicitation" | "askuser" => ParsedToolKind::Elicitation,
        _ => ParsedToolKind::Tool,
    }
}

fn generated_id(sequence: usize) -> String {
    format!("gemini_call_{sequence}")
}

pub fn parse_tool_calls(text: &str) -> (String, Vec<ParsedToolCall>) {
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

    for cap in re_tool_call.captures_iter(&clean) {
        if let Some(call) = parse_single(cap[1].trim(), calls.len()) {
            calls.push(call);
        }
    }
    clean = re_tool_call.replace_all(&clean, "").trim().to_string();

    for cap in re_func_call.captures_iter(&clean) {
        if let Some(call) = parse_single_func(cap[1].trim(), calls.len()) {
            calls.push(call);
        }
    }
    clean = re_func_call.replace_all(&clean, "").trim().to_string();

    if calls.is_empty() && clean.trim_start().starts_with('{') {
        if let Ok(data) = serde_json::from_str::<Value>(clean.trim()) {
            if let Some(name) = data.get("name").and_then(Value::as_str) {
                let args = data
                    .get("arguments")
                    .or_else(|| data.get("args"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                calls.push(ParsedToolCall::new(generated_id(0), name, args));
                clean.clear();
            }
        }
    }

    (clean, calls)
}

fn parse_single(raw: &str, sequence: usize) -> Option<ParsedToolCall> {
    let data: Value = serde_json::from_str(raw).ok()?;
    let name = data.get("name").and_then(Value::as_str)?;
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| generated_id(sequence));
    let arguments = data.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Some(ParsedToolCall::new(id, name, arguments))
}

fn parse_single_func(raw: &str, sequence: usize) -> Option<ParsedToolCall> {
    let data: Value = serde_json::from_str(raw).ok()?;
    let name = data.get("name").and_then(Value::as_str)?;
    let id = data
        .get("id")
        .or_else(|| data.get("call_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| generated_id(sequence));
    let arguments = data
        .get("args")
        .or_else(|| data.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some(ParsedToolCall::new(id, name, arguments))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_model_call_id() {
        let (_, calls) = parse_tool_calls("```tool_call\n{\"id\":\"abc\",\"name\":\"file_read\",\"arguments\":{}}\n```");
        assert_eq!(calls[0].id, "abc");
    }

    #[test]
    fn generates_stable_sequence_id() {
        let text = "```tool_call\n{\"name\":\"file_read\",\"arguments\":{}}\n```\n```tool_call\n{\"name\":\"search\",\"arguments\":{}}\n```";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls[0].id, "gemini_call_0");
        assert_eq!(calls[1].id, "gemini_call_1");
    }

    #[test]
    fn detects_gemini_elicitation() {
        let text = "```function_call\n{\"name\":\"AskUserQuestion\",\"args\":{\"questions\":[{\"question\":\"Quel langage ?\",\"options\":[{\"label\":\"Rust\"}]}]}}\n```";
        let (_, calls) = parse_tool_calls(text);
        assert!(calls[0].is_elicitation());
    }

    #[test]
    fn history_contains_identity() {
        let call = ParsedToolCall::new("abc", "file_read", json!({}));
        assert!(call.to_history_block().contains("\"id\":\"abc\""));
    }

    #[test]
    fn ordinary_tool_is_not_elicitation() {
        let call = ParsedToolCall::new("abc", "shell_exec", json!({}));
        assert!(!call.is_elicitation());
    }
}
