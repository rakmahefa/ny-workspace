//! Parsing normalisé des appels d'outils depuis les réponses texte Gemini.
//!
//! The parser turns fenced tool calls and Gemini's native `<FollowUp>`
//! component into the same ACP-oriented `ParsedToolCall` representation.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedToolKind {
    Tool,
    Elicitation,
}

impl ParsedToolKind {
    pub fn is_elicitation(self) -> bool { matches!(self, Self::Elicitation) }
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
        let original_name = name.into();
        let kind = classify_tool_kind(&original_name);
        let name = if kind.is_elicitation() { "AskUserQuestion".to_owned() } else { original_name };
        Self { id: id.into(), name, arguments, kind }
    }

    pub fn is_elicitation(&self) -> bool { self.kind.is_elicitation() }

    pub fn to_history_block(&self) -> String {
        format!("```tool_call\n{}\n```", json!({"id": self.id, "name": self.name, "arguments": self.arguments}))
    }
}

fn classify_tool_kind(name: &str) -> ParsedToolKind {
    let normalized = name.trim().to_ascii_lowercase().replace(['-', '_'], "");
    match normalized.as_str() {
        "askuserquestion" | "elicitation" | "askuser" => ParsedToolKind::Elicitation,
        _ => ParsedToolKind::Tool,
    }
}

fn generated_id(sequence: usize) -> String { format!("gemini_call_{sequence}") }

pub fn parse_tool_calls(text: &str) -> (String, Vec<ParsedToolCall>) {
    static RE_TOOL_CALL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_FUNC_CALL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

    let re_tool_call = RE_TOOL_CALL.get_or_init(|| regex::Regex::new(r"(?s)```tool_call\s*\n(.*?)\n```").expect("regex statique"));
    let re_func_call = RE_FUNC_CALL.get_or_init(|| regex::Regex::new(r"(?s)```function_call\s*\n(.*?)\n```").expect("regex statique"));

    let mut calls = Vec::new();
    let mut clean = text.to_string();

    for cap in re_tool_call.captures_iter(&clean) {
        if let Some(call) = parse_single(cap[1].trim(), calls.len()) { calls.push(call); }
    }
    clean = re_tool_call.replace_all(&clean, "").trim().to_string();

    for cap in re_func_call.captures_iter(&clean) {
        if let Some(call) = parse_single_func(cap[1].trim(), calls.len()) { calls.push(call); }
    }
    clean = re_func_call.replace_all(&clean, "").trim().to_string();

    let (without_follow_up, follow_up) = extract_follow_up(&clean);
    clean = without_follow_up;
    if let Some((label, query)) = follow_up {
        calls.push(ParsedToolCall::new(
            generated_id(calls.len()),
            "FollowUp",
            json!({"label": label, "query": query}),
        ));
    }

    if calls.is_empty() && clean.trim_start().starts_with('{') {
        if let Ok(data) = serde_json::from_str::<Value>(clean.trim()) {
            if let Some(name) = data.get("name").and_then(Value::as_str) {
                let args = data.get("arguments").or_else(|| data.get("args")).cloned().unwrap_or_else(|| json!({}));
                calls.push(ParsedToolCall::new(generated_id(0), name, args));
                clean.clear();
            }
        }
    }

    (clean, calls)
}

fn extract_follow_up(text: &str) -> (String, Option<(String, String)>) {
    const MARKER: &str = "<FollowUp";
    let mut clean = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut found = None;

    while let Some(relative_start) = text[cursor..].find(MARKER) {
        let start = cursor + relative_start;
        clean.push_str(&text[cursor..start]);

        let Some(relative_end) = find_tag_end(&text[start + MARKER.len()..]) else {
            clean.push_str(&text[start..]);
            cursor = text.len();
            break;
        };
        let end = start + MARKER.len() + relative_end;
        let tag = &text[start..=end];

        if let Some((label, query)) = parse_follow_up_tag(tag) {
            if found.is_none() { found = Some((label, query)); }
        } else {
            clean.push_str(tag);
        }
        cursor = end + 1;
    }

    clean.push_str(&text[cursor..]);
    (clean.trim().to_owned(), found)
}

fn find_tag_end(input: &str) -> Option<usize> {
    let mut quote = None;
    for (index, byte) in input.as_bytes().iter().copied().enumerate() {
        match quote {
            Some(current) if byte == current => quote = None,
            Some(_) => {}
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            None if byte == b'>' => return Some(index),
            None => {}
        }
    }
    None
}

fn parse_follow_up_tag(tag: &str) -> Option<(String, String)> {
    let inner = tag.strip_prefix("<FollowUp")?.strip_suffix('>')?.trim();
    let inner = inner.strip_suffix('/').unwrap_or(inner).trim();
    let attrs = parse_attributes(inner);
    let label = attrs.get("label")?.trim();
    let query = attrs.get("query")?.trim();
    if label.is_empty() || query.is_empty() { return None; }
    Some((decode_xml(label), decode_xml(query)))
}

fn parse_attributes(input: &str) -> std::collections::BTreeMap<String, String> {
    let mut attrs = std::collections::BTreeMap::new();
    let bytes = input.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() { index += 1; }
        if index >= bytes.len() || bytes[index] == b'/' { break; }

        let key_start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'=' { index += 1; }
        if key_start == index { index += 1; continue; }
        let key = &input[key_start..index];

        while index < bytes.len() && bytes[index].is_ascii_whitespace() { index += 1; }
        if index >= bytes.len() || bytes[index] != b'=' { break; }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() { index += 1; }
        if index >= bytes.len() { break; }

        let value = if bytes[index] == b'\'' || bytes[index] == b'"' {
            let quote = bytes[index];
            index += 1;
            let value_start = index;
            while index < bytes.len() && bytes[index] != quote { index += 1; }
            let value = input[value_start..index].to_owned();
            if index < bytes.len() { index += 1; }
            value
        } else {
            let value_start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() { index += 1; }
            input[value_start..index].to_owned()
        };
        attrs.insert(key.to_ascii_lowercase(), value);
    }
    attrs
}

fn decode_xml(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn parse_single(raw: &str, sequence: usize) -> Option<ParsedToolCall> {
    let data: Value = serde_json::from_str(raw).ok()?;
    let name = data.get("name").and_then(Value::as_str)?;
    let id = data.get("id").and_then(Value::as_str).filter(|id| !id.trim().is_empty()).map(ToOwned::to_owned).unwrap_or_else(|| generated_id(sequence));
    let arguments = data.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Some(ParsedToolCall::new(id, name, arguments))
}

fn parse_single_func(raw: &str, sequence: usize) -> Option<ParsedToolCall> {
    let data: Value = serde_json::from_str(raw).ok()?;
    let name = data.get("name").and_then(Value::as_str)?;
    let id = data.get("id").or_else(|| data.get("call_id")).and_then(Value::as_str).filter(|id| !id.trim().is_empty()).map(ToOwned::to_owned).unwrap_or_else(|| generated_id(sequence));
    let arguments = data.get("args").or_else(|| data.get("arguments")).cloned().unwrap_or_else(|| json!({}));
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
    fn detects_and_normalizes_gemini_elicitation() {
        let text = "```function_call\n{\"name\":\"ask_user_question\",\"args\":{\"questions\":[{\"question\":\"Quel langage ?\",\"options\":[{\"label\":\"Rust\"}]}]}}\n```";
        let (_, calls) = parse_tool_calls(text);
        assert!(calls[0].is_elicitation());
        assert_eq!(calls[0].name, "AskUserQuestion");
    }

    #[test]
    fn parses_follow_up_as_tool_call() {
        let text = r#"Quel type de projet ? <FollowUp label="Initialiser un nouveau projet" query="Initialisons un nouveau projet dans cet espace de travail." />"#;
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(clean, "Quel type de projet ?");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "FollowUp");
        assert_eq!(calls[0].arguments["label"], "Initialiser un nouveau projet");
        assert_eq!(calls[0].arguments["query"], "Initialisons un nouveau projet dans cet espace de travail.");
    }

    #[test]
    fn parses_follow_up_with_reordered_attributes() {
        let text = r#"<FollowUp query='cargo test' label='Run tests'/>"#;
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(clean, "");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["label"], "Run tests");
        assert_eq!(calls[0].arguments["query"], "cargo test");
    }

    #[test]
    fn parses_multiline_follow_up_and_xml_entities() {
        let text = "prefix\n<FollowUp\n  query=\"echo &gt; test\"\n  label=\"Show &amp; verify\"\n/>\nsuffix";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(clean, "prefix\nsuffix");
        assert_eq!(calls[0].arguments["label"], "Show & verify");
        assert_eq!(calls[0].arguments["query"], "echo > test");
    }

    #[test]
    fn preserves_invalid_follow_up_as_text() {
        let text = "hello <FollowUp label=\"Only label\" /> world";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(clean, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn only_one_follow_up_is_parsed() {
        let text = r#"<FollowUp label="One" query="1" /><FollowUp label="Two" query="2" />"#;
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["label"], "One");
    }

    #[test]
    fn history_contains_identity() {
        let call = ParsedToolCall::new("abc", "file_read", json!({}));
        assert!(call.to_history_block().contains("\"id\":\"abc\""));
    }
}
