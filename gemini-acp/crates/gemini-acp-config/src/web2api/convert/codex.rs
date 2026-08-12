//! Format Codex CLI `/v1/responses` (refactor M9 §6.4).
//!
//! Port de `handle_responses` : `input` (chaîne ou items) → messages OpenAI,
//! `instructions` → system, tools normalisés.

use serde_json::{json, Value};

use super::openai::content_text;

/// Normalise les tools de `/v1/responses` (items `{type: function, name, …}` →
/// `{type: function, function: {…}}`), comme le vendor.
pub fn normalize_responses_tools(tools: Option<&Value>) -> Vec<Value> {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .map(|t| {
            if t.get("type").and_then(Value::as_str) == Some("function")
                && t.get("function").is_none()
            {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").and_then(Value::as_str).unwrap_or(""),
                        "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                        "parameters": t.get("parameters").cloned().unwrap_or_else(|| json!({})),
                    }
                })
            } else {
                t.clone()
            }
        })
        .collect()
}

/// Convertit `input` de `/v1/responses` (chaîne ou items) en messages OpenAI.
/// Port de `handle_responses` : `function_call_output` → tool, messages
/// assistant avec `function_call` → `tool_calls`, autres → user.
pub fn responses_input_to_messages(req: &Value) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(instructions) = req.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    let input = req.get("input").cloned().unwrap_or_else(|| json!([]));
    match input {
        Value::String(s) => messages.push(json!({"role": "user", "content": s})),
        Value::Array(items) => {
            for item in items {
                match &item {
                    Value::String(s) => {
                        messages.push(json!({"role": "user", "content": s}));
                    }
                    Value::Object(_)
                        if item.get("type").and_then(Value::as_str)
                            == Some("function_call_output") =>
                    {
                        messages.push(json!({
                            "role": "tool",
                            "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                            "content": item.get("output").and_then(Value::as_str).unwrap_or(""),
                        }));
                    }
                    Value::Object(_) => {
                        let is_assistant = item.get("role").and_then(Value::as_str)
                            == Some("assistant")
                            || (item.get("type").and_then(Value::as_str) == Some("message")
                                && item.get("role").and_then(Value::as_str) == Some("assistant"));
                        if is_assistant {
                            let (text, tool_calls) = match item.get("content") {
                                Some(Value::Array(content)) => {
                                    let mut text = String::new();
                                    let mut tcs = Vec::new();
                                    for c in content {
                                        match c.get("type").and_then(Value::as_str) {
                                            Some("output_text") => {
                                                if let Some(t) =
                                                    c.get("text").and_then(Value::as_str)
                                                {
                                                    text.push_str(t);
                                                }
                                            }
                                            Some("function_call") => tcs.push(c.clone()),
                                            _ => {}
                                        }
                                    }
                                    (text, tcs)
                                }
                                Some(Value::String(s)) => (s.clone(), Vec::new()),
                                _ => (String::new(), Vec::new()),
                            };
                            let mut m = json!({"role": "assistant", "content": text});
                            if !tool_calls.is_empty() {
                                let calls: Vec<Value> = tool_calls
                                    .iter()
                                    .enumerate()
                                    .map(|(i, tc)| {
                                        json!({
                                            "id": tc.get("call_id").and_then(Value::as_str)
                                                .map(|s| s.to_string())
                                                .unwrap_or_else(|| format!("call_{i}")),
                                            "type": "function",
                                            "function": {
                                                "name": tc.get("name").and_then(Value::as_str).unwrap_or(""),
                                                "arguments": tc.get("arguments")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or("{}"),
                                            }
                                        })
                                    })
                                    .collect();
                                m["tool_calls"] = Value::Array(calls);
                            }
                            messages.push(m);
                        } else {
                            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                            let content = content_text(item.get("content").unwrap_or(&Value::Null));
                            messages.push(json!({"role": role, "content": content}));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    messages
}
