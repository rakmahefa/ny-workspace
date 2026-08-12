//! Format Google natif `/v1beta/models` (refactor M9 §6.4).
//!
//! Port de `_google_contents_to_prompt` et `parse_google_function_calls` :
//! `contents` (role user/model), `parts` (`text`, `inlineData` → upload Scotty,
//! `functionCall` historique → bloc, `functionResponse` → `[Tool result for …]`),
//! `systemInstruction` → system, `tools.functionDeclarations` → section
//! `# Tool Use` au format `function_call` + contrainte `toolConfig`.

use serde_json::{json, Value};

use gemini_acp_config::core::tool_prompt::{
    tool_result_line, tool_use_section, BlockKind, INSTRUCTION_FUNCTION_CALL,
};

/// Bloc ` ```function_call\n{"name": …, "args": …}\n``` ` (format de sortie
/// Google natif — cf. §5.2). Port de `tools.py` (historique `functionCall`).
fn function_call_block(fc: &Value) -> String {
    format!(
        "```function_call\n{}\n```",
        json!({
            "name": fc.get("name").and_then(Value::as_str).unwrap_or(""),
            "args": fc.get("args").cloned().unwrap_or_else(|| json!({})),
        })
    )
}

/// Déclarations `tools[].functionDeclarations` → descripteurs `{name,
/// description, parameters?}` (port du vendor, `parametersJsonSchema` toléré).
fn google_tool_defs(req: &Value) -> Vec<Value> {
    let mut defs = Vec::new();
    if let Some(tools) = req.get("tools").and_then(Value::as_array) {
        for group in tools {
            if let Some(fns) = group.get("functionDeclarations").and_then(Value::as_array) {
                for fn_ in fns {
                    let mut td = json!({
                        "name": fn_.get("name").and_then(Value::as_str).unwrap_or(""),
                        "description": fn_.get("description").and_then(Value::as_str).unwrap_or(""),
                    });
                    let params = fn_
                        .get("parameters")
                        .or_else(|| fn_.get("parametersJsonSchema"));
                    if let Some(p) = params {
                        td["parameters"] = p.clone();
                    }
                    defs.push(td);
                }
            }
        }
    }
    defs
}

/// Section `# Tool Use` du format Google natif : blocs `function_call`
/// (port de `tools.py::build_tool_prompt`).
fn google_tools_section(defs: &[Value]) -> String {
    tool_use_section(BlockKind::FunctionCall, INSTRUCTION_FUNCTION_CALL, defs, "")
}

/// Contrainte `toolConfig.functionCallingConfig` (mode + allowedFunctionNames) —
/// port de `_google_tool_choice_instruction`.
fn google_tool_choice_instruction(req: &Value) -> String {
    let config = req
        .get("toolConfig")
        .and_then(|c| c.get("functionCallingConfig"));
    let mode = config
        .and_then(|c| c.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("AUTO");
    match mode {
        "NONE" => "\n\nIMPORTANT: Do NOT call any tools. Respond with text only.".into(),
        "ANY" => {
            let allowed: Vec<String> = config
                .and_then(|c| c.get("allowedFunctionNames"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            if allowed.is_empty() {
                "\n\nIMPORTANT: You MUST call at least one tool. Do not respond with text only."
                    .into()
            } else {
                format!(
                    "\n\nIMPORTANT: You MUST call one of these tools: {}. Do not respond with text only.",
                    allowed
                        .iter()
                        .map(|n| format!("\"{n}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        _ => String::new(),
    }
}

/// `contents` Google (`generateContent`) → prompt texte + **images**
/// (`inlineData`, base64 + mime — upload Scotty ensuite). Port de
/// `_google_contents_to_prompt` : `systemInstruction` → `[System instruction]`,
/// role `model` → `[Assistant]`, autres → texte brut ; outils
/// `functionDeclarations` → section `# Tool Use` au format `function_call`
/// (+ contrainte `toolConfig`) ; historique `functionCall` → bloc, `functionResponse`
/// → `[Tool result for …]`.
pub fn google_contents_to_prompt(req: &Value) -> (String, Vec<(String, String)>) {
    let mut parts = Vec::new();
    let mut images = Vec::new();

    let tool_defs = google_tool_defs(req);
    let sys_text = req
        .get("systemInstruction")
        .map(|sys| parts_text(sys.get("parts"), &mut Vec::new()))
        .unwrap_or_default();

    if !sys_text.is_empty() {
        if tool_defs.is_empty() {
            parts.push(format!("[System instruction]: {sys_text}"));
        } else {
            parts.push(format!(
                "[System instruction]: {sys_text}\n\n{}{}",
                google_tools_section(&tool_defs),
                google_tool_choice_instruction(req)
            ));
        }
    } else if !tool_defs.is_empty() {
        parts.push(format!(
            "{}{}",
            google_tools_section(&tool_defs),
            google_tool_choice_instruction(req)
        ));
    }

    if let Some(contents) = req.get("contents").and_then(Value::as_array) {
        for content in contents {
            let role = content
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let text = parts_text(content.get("parts"), &mut images);
            match role {
                "model" => parts.push(format!("[Assistant]: {text}")),
                _ => {
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
        }
    }
    (parts.join("\n\n"), images)
}

/// Texte des parts Google (`text`, `functionCall` → bloc, `functionResponse` →
/// résultat, `inlineData` → image extraite en `(base64, mime)`).
fn parts_text(parts: Option<&Value>, images: &mut Vec<(String, String)>) -> String {
    let Some(parts) = parts.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = Vec::new();
    for p in parts {
        match p.get("text").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => out.push(t.to_string()),
            _ => {
                if let Some(fc) = p.get("functionCall") {
                    out.push(function_call_block(fc));
                } else if let Some(id) = p.get("inlineData") {
                    images.push((
                        id.get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        id.get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string(),
                    ));
                } else if let Some(fr) = p.get("functionResponse") {
                    out.push(tool_result_line(
                        fr.get("name").and_then(Value::as_str).unwrap_or(""),
                        &serde_json::json!(fr.get("response")).to_string(),
                    ));
                }
            }
        }
    }
    out.join(" ")
}

/// Extrait les appels `function_call` de la réponse Google (3 formats du
/// vendor : bloc ` ```function_call `, ligne `function_call`, puis JSON brut
/// `{"name", "args"}` isolé). Retourne `(texte nettoyé, [{name, args}])`.
pub fn parse_google_function_calls(text: &str) -> (String, Vec<Value>) {
    // Q3 : les regex sont statiques — compilation unique via `OnceLock`.
    static RE1: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE2: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re1 = RE1.get_or_init(|| {
        regex::Regex::new(r"(?s)```function_call\s*\n(.*?)\n```").expect("regex statique")
    });
    // Format 2 (sans backticks) : JSON sur une ligne — `.*` glouton jusqu'au
    // dernier `}` de la ligne supporte les `args` imbriqués (port du vendor).
    let re2 = RE2.get_or_init(|| {
        regex::Regex::new(r"(?m)(?:^|\n)function_call\s*\n(\{.*\})(?:\n|$)")
            .expect("regex statique")
    });
    let mut calls = Vec::new();
    let mut clean = text.to_string();
    for re in [re1, re2] {
        for cap in re.captures_iter(&clean) {
            let Ok(data) = serde_json::from_str::<Value>(cap[1].trim()) else {
                continue;
            };
            let Some(name) = data.get("name").and_then(Value::as_str) else {
                continue;
            };
            calls.push(json!({
                "name": name,
                "args": data.get("args")
                    .or_else(|| data.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            }));
        }
        clean = re.replace_all(&clean, "").trim().to_string();
    }
    // Format 3 : JSON brut isolé.
    if calls.is_empty() && clean.trim_start().starts_with('{') {
        if let Ok(data) = serde_json::from_str::<Value>(clean.trim()) {
            if let Some(name) = data.get("name").and_then(Value::as_str) {
                if data.get("args").is_some() || data.get("arguments").is_some() {
                    calls.push(json!({
                        "name": name,
                        "args": data.get("args")
                            .or_else(|| data.get("arguments"))
                            .cloned()
                            .unwrap_or_else(|| json!({})),
                    }));
                    clean.clear();
                }
            }
        }
    }
    (clean, calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_contents_extrait_les_images_inline() {
        let req = json!({
            "contents": [{"role": "user", "parts": [
                {"text": "que vois-tu ?"},
                {"inlineData": {"mimeType": "image/jpeg", "data": "aGVsbG8="}},
            ]}]
        });
        let (prompt, images) = google_contents_to_prompt(&req);
        assert!(prompt.contains("que vois-tu ?"));
        assert_eq!(
            images,
            vec![("aGVsbG8=".to_string(), "image/jpeg".to_string())]
        );
    }

    #[test]
    fn google_tools_section_function_call() {
        let req = json!({
            "systemInstruction": {"parts": [{"text": "sois utile"}]},
            "tools": [{"functionDeclarations": [{
                "name": "lire", "description": "lit un fichier",
                "parameters": {"type": "object"}
            }]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["lire"]}},
            "contents": [{"role": "user", "parts": [{"text": "lis /etc/hosts"}]}]
        });
        let (prompt, images) = google_contents_to_prompt(&req);
        assert!(images.is_empty());
        assert!(prompt.contains("```function_call"));
        assert!(prompt.contains("\"lire\""));
        assert!(prompt.contains("MUST call one of these tools: \"lire\""));
    }

    #[test]
    fn parse_google_function_calls_trois_formats() {
        // Bloc standard.
        let t = "texte\n```function_call\n{\"name\": \"a\", \"args\": {\"x\": 1}}\n```";
        let (clean, calls) = parse_google_function_calls(t);
        assert!(clean.contains("texte"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "a");
        assert_eq!(calls[0]["args"]["x"], 1);
        // Sans backticks (format 2).
        let t2 = "function_call\n{\"name\": \"b\", \"args\": {}}";
        let (_, calls2) = parse_google_function_calls(t2);
        assert_eq!(calls2.len(), 1);
        assert_eq!(calls2[0]["name"], "b");
        // JSON brut isolé (format 3) + tolérance `arguments`.
        let t3 = "{\"name\": \"c\", \"arguments\": {\"y\": 2}}";
        let (clean3, calls3) = parse_google_function_calls(t3);
        assert!(clean3.is_empty());
        assert_eq!(calls3.len(), 1);
        assert_eq!(calls3[0]["args"]["y"], 2);
        // Sans appel → texte inchangé.
        let (clean4, calls4) = parse_google_function_calls("réponse simple");
        assert_eq!(clean4, "réponse simple");
        assert!(calls4.is_empty());
    }
}
