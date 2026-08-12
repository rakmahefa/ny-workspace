//! Format OpenAI `/v1/chat/completions` (refactor M9 §6.4).
//!
//! Port de `messages_to_prompt` : system → `[System instruction]`, assistant
//! avec `tool_calls` → blocs ` ```tool_call `, tool → `[Tool result for <name>]`,
//! user → texte. `tool_choice` `none` → pas de section tools.

use serde_json::{json, Value};

use gemini_acp_config::core::tool_prompt::{tool_result_line, tool_use_section, BlockKind};

use super::common::{tool_call_block, ToolChoice};

/// Instruction de la section tools OpenAI (fidélité vendor) — se termine par
/// `Available tools:\n`. Le préfixe `[System instruction]:` reste concaténé
/// par l'appelant.
const INSTRUCTION_OPENAI: &str = "You have access to tools. To call a tool, respond with:\n\
```tool_call\n{\"name\": \"func_name\", \"arguments\": {{...}}}\n```\n\
Only use tool_call blocks when needed.\n\n\
Available tools:\n";

/// Section `# Tool Use` (déclarations de fonctions) — port de
/// `messages_to_prompt` (bloc tools) : consigne + liste JSON des descripteurs
/// + contrainte `tool_choice` (port de `_build_tool_choice_instruction`).
pub fn tools_section(tools: &[Value], tool_choice: &ToolChoice) -> Option<String> {
    let mut defs = Vec::new();
    for tool in tools {
        let fn_ = if tool.get("type").and_then(Value::as_str) == Some("function") {
            tool.get("function").cloned()
        } else {
            Some(tool.clone())
        };
        if let Some(f) = fn_ {
            defs.push(json!({
                "name": f.get("name").and_then(Value::as_str).unwrap_or(""),
                "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                "parameters": f.get("parameters").cloned().unwrap_or_else(|| json!({})),
            }));
        }
    }
    if defs.is_empty() {
        return None;
    }
    Some(format!(
        "[System instruction]: {}",
        tool_use_section(
            BlockKind::ToolCall,
            INSTRUCTION_OPENAI,
            &defs,
            &tool_choice.instruction()
        )
    ))
}

/// Texte d'un message OpenAI `content` : chaîne, ou liste de parts
/// `{type: text|input_text}` (+ note si une image est présente).
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            out.push(t.to_string());
                        }
                    }
                    Some("image_url") => out.push(
                        "[Note: Image input not supported by this server; describe the image in text]. "
                            .to_string(),
                    ),
                    _ => {}
                }
            }
            out.join(" ")
        }
        _ => String::new(),
    }
}

/// Convertit les messages OpenAI en prompt texte. Port de `messages_to_prompt` :
/// system → `[System instruction]: …`, assistant avec `tool_calls` → blocs
/// ` ```tool_call `, tool → `[Tool result for <name>]: <content>`, user → texte.
/// `tool_choice` `none` → pas de section tools.
pub fn messages_to_prompt(
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &ToolChoice,
) -> String {
    let mut parts = Vec::new();
    if let Some(tools) = tools {
        if !tool_choice.is_none() {
            if let Some(section) = tools_section(tools, tool_choice) {
                parts.push(section);
            }
        }
    }
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = content_text(msg.get("content").unwrap_or(&Value::Null));
        match role {
            "system" => parts.push(format!("[System instruction]: {content}")),
            "assistant" => match msg.get("tool_calls") {
                Some(Value::Array(tcs)) if !tcs.is_empty() => {
                    let mut blocks = Vec::new();
                    for tc in tcs {
                        let f = tc.get("function").cloned().unwrap_or_else(|| json!({}));
                        let name = f.get("name").and_then(Value::as_str).unwrap_or("");
                        let args: Value = f
                            .get("arguments")
                            .and_then(|a| serde_json::from_str(a.as_str()?).ok())
                            .unwrap_or_else(|| json!({}));
                        blocks.push(tool_call_block(name, &args));
                    }
                    let prefix = if content.is_empty() {
                        String::new()
                    } else {
                        format!("{content}\n")
                    };
                    parts.push(format!("[Assistant]: {prefix}{}", blocks.join("\n")));
                }
                _ => parts.push(format!("[Assistant]: {content}")),
            },
            "tool" => {
                let name = msg.get("name").and_then(Value::as_str).unwrap_or("");
                parts.push(tool_result_line(name, &content));
            }
            _ => {
                if !content.is_empty() {
                    parts.push(content);
                }
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_choice_none_occulte_la_section_tools() {
        let tools = vec![json!({"type": "function", "function": {
            "name": "lire", "description": "d", "parameters": {}}})];
        let prompt = messages_to_prompt(&[], Some(&tools), &ToolChoice::None);
        assert!(!prompt.contains("You have access to tools"));
        let prompt = messages_to_prompt(&[], Some(&tools), &ToolChoice::Required);
        assert!(prompt.contains("You have access to tools"));
        assert!(prompt.contains("MUST call at least one tool"));
    }
}
