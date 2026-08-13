//! Shared ACP elicitation bridge used by interactive tools.
//!
//! This module isolates the protocol projection from tool execution. It follows
//! the same separation used by `claude-agent-acp/src/elicitation.ts`: validate
//! model-authored questions, build a bounded ACP form schema, wait for the
//! user's response, and fold the response back into the tool's input shape.
//!
//! The implementation is intentionally Rust-native rather than a line-by-line
//! translation of the upstream TypeScript. Complex ACP schema values are
//! validated through the protocol's serde representation so this layer remains
//! resilient to non-exhaustive schema additions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, ElicitationAction, ElicitationContentValue, ElicitationFormMode,
    ElicitationPropertySchema, ElicitationSchema, ElicitationSessionScope, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde::Deserialize;
use serde_json::{json, Value};

const QUESTION_KEY_PREFIX: &str = "question_";
const CUSTOM_ANSWER_SUFFIX: &str = "_custom";
const OPTION_PREVIEW_META: &str = "_claude/askUserQuestionOption";
const CUSTOM_ANSWER_META: &str = "_askUserQuestionCustomAnswer";
const MAX_QUESTION_COUNT: usize = 16;
const MAX_OPTIONS_PER_QUESTION: usize = 16;
const MAX_TEXT_CHARS: usize = 4_096;

#[derive(Debug, Clone, Deserialize)]
pub struct AskUserQuestion {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    pub options: Vec<AskUserOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskUserOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AskUserInput {
    questions: Vec<AskUserQuestion>,
}

pub fn parse_questions(input: &Value) -> Result<Vec<AskUserQuestion>, String> {
    let parsed: AskUserInput = serde_json::from_value(input.clone())
        .map_err(|error| format!("invalid AskUserQuestion input: {error}"))?;

    if parsed.questions.is_empty() {
        return Err("AskUserQuestion requires at least one question.".to_string());
    }
    if parsed.questions.len() > MAX_QUESTION_COUNT {
        return Err(format!(
            "AskUserQuestion supports at most {MAX_QUESTION_COUNT} questions."
        ));
    }

    for (index, question) in parsed.questions.iter().enumerate() {
        if question.question.trim().is_empty() {
            return Err(format!("question {index} is empty"));
        }
        if question.options.is_empty() {
            return Err(format!("question {index} has no options"));
        }
        if question.options.len() > MAX_OPTIONS_PER_QUESTION {
            return Err(format!(
                "question {index} supports at most {MAX_OPTIONS_PER_QUESTION} options"
            ));
        }
        for option in &question.options {
            if option.label.trim().is_empty() {
                return Err(format!("question {index} contains an empty option label"));
            }
        }
    }

    Ok(parsed.questions)
}

pub async fn request_user_input(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    questions: &[AskUserQuestion],
) -> Result<String, String> {
    let properties = build_question_properties(questions)?;
    let mut schema = ElicitationSchema::new();
    for (name, property) in properties {
        schema = schema.property(name, property, false);
    }

    let message = if questions.len() == 1 {
        truncate(&questions[0].question)
    } else {
        "Please answer the following questions.".to_string()
    };

    let mode = ElicitationFormMode::new(ElicitationSessionScope::new(session_id.clone()), schema);
    let request = CreateElicitationRequest::new(mode, message);

    let response = cx
        .send_request(request)
        .block_task()
        .await
        .map_err(|error| format!("ACP elicitation request failed: {error}"))?;

    match response.action {
        ElicitationAction::Accept(accept) => {
            let answers = fold_answers(accept.content.unwrap_or_default(), questions);
            Ok(json!({ "answers": answers }).to_string())
        }
        ElicitationAction::Decline => Ok(json!({ "answers": {} }).to_string()),
        ElicitationAction::Cancel => Err("user cancelled AskUserQuestion".to_string()),
        _ => Err("ACP returned an unsupported elicitation action".to_string()),
    }
}

pub fn build_question_properties(
    questions: &[AskUserQuestion],
) -> Result<BTreeMap<String, ElicitationPropertySchema>, String> {
    let mut properties = BTreeMap::new();
    let single_question = questions.len() == 1;

    for (index, question) in questions.iter().enumerate() {
        let options: Vec<Value> = question
            .options
            .iter()
            .map(|option| {
                let mut value = json!({
                    "const": truncate(&option.label),
                    "title": truncate(&option.label),
                });
                if let Some(description) = &option.description {
                    value["description"] = Value::String(truncate(description));
                }
                if let Some(preview) = &option.preview {
                    value["_meta"] = json!({
                        OPTION_PREVIEW_META: { "preview": truncate(preview) }
                    });
                }
                value
            })
            .collect();

        let field_key = question_key(index);
        let description = (!single_question).then(|| truncate(&question.question));
        let title = question.header.as_deref().map(truncate);

        let property_json = if question.multi_select {
            json!({
                "type": "array",
                "title": title,
                "description": description,
                "items": { "anyOf": options }
            })
        } else {
            json!({
                "type": "string",
                "title": title,
                "description": description,
                "oneOf": options
            })
        };

        let property: ElicitationPropertySchema = serde_json::from_value(property_json)
            .map_err(|error| format!("invalid ACP elicitation schema for {field_key}: {error}"))?;
        properties.insert(field_key.clone(), property);

        let custom_key = custom_answer_key(index);
        let custom: ElicitationPropertySchema = serde_json::from_value(json!({
            "type": "string",
            "title": "Other",
            "description": "Type your own answer instead of choosing an option above (optional).",
            "_meta": {
                CUSTOM_ANSWER_META: {
                    "questionId": field_key,
                    "isCustomAnswer": true
                }
            }
        }))
        .map_err(|error| format!("invalid ACP custom-answer schema for {custom_key}: {error}"))?;
        properties.insert(custom_key, custom);
    }

    Ok(properties)
}

pub fn fold_answers(
    content: BTreeMap<String, ElicitationContentValue>,
    questions: &[AskUserQuestion],
) -> serde_json::Map<String, Value> {
    let mut answers = serde_json::Map::new();

    for (index, question) in questions.iter().enumerate() {
        if let Some(ElicitationContentValue::String(custom)) = content.get(&custom_answer_key(index)) {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                answers.insert(question.question.clone(), Value::String(trimmed.to_string()));
                continue;
            }
        }

        let key = question_key(index);
        if let Some(value) = content.get(&key) {
            match value {
                ElicitationContentValue::String(text) if !text.trim().is_empty() => {
                    answers.insert(question.question.clone(), Value::String(text.clone()));
                }
                ElicitationContentValue::StringArray(values) if !values.is_empty() => {
                    answers.insert(question.question.clone(), json!(values));
                }
                ElicitationContentValue::Boolean(value) => {
                    answers.insert(question.question.clone(), Value::Bool(*value));
                }
                ElicitationContentValue::Number(value) => {
                    answers.insert(question.question.clone(), json!(value));
                }
                _ => {}
            }
        }
    }

    answers
}

fn question_key(index: usize) -> String {
    format!("{QUESTION_KEY_PREFIX}{index}")
}

fn custom_answer_key(index: usize) -> String {
    format!("{}{}", question_key(index), CUSTOM_ANSWER_SUFFIX)
}

fn truncate(value: &str) -> String {
    let mut chars = value.chars();
    let preview: String = chars.by_ref().take(MAX_TEXT_CHARS).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

pub fn current_working_directory(cwd: &Path) -> PathBuf {
    cwd.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(multi_select: bool) -> AskUserQuestion {
        AskUserQuestion {
            question: "Quel langage ?".into(),
            header: Some("Langage".into()),
            multi_select,
            options: vec![
                AskUserOption {
                    label: "Rust".into(),
                    description: Some("Sûr et rapide".into()),
                    preview: Some("fn main() {}".into()),
                },
                AskUserOption {
                    label: "Python".into(),
                    description: None,
                    preview: None,
                },
            ],
        }
    }

    #[test]
    fn parses_and_validates_questions() {
        let input = json!({"questions": [serde_json::to_value(question(false)).unwrap()]});
        let parsed = parse_questions(&input).expect("questions");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn rejects_empty_questions() {
        let error = parse_questions(&json!({"questions": []})).unwrap_err();
        assert!(error.contains("at least one"));
    }

    #[test]
    fn builds_single_and_multi_select_fields() {
        let single = build_question_properties(&[question(false)]).expect("single schema");
        assert_eq!(single.len(), 2);

        let multi = build_question_properties(&[question(true)]).expect("multi schema");
        assert_eq!(multi.len(), 2);
    }

    #[test]
    fn custom_answer_has_priority() {
        let content = BTreeMap::from([
            (
                question_key(0),
                ElicitationContentValue::String("Rust".into()),
            ),
            (
                custom_answer_key(0),
                ElicitationContentValue::String("  Go  ".into()),
            ),
        ]);
        let answers = fold_answers(content, &[question(false)]);
        assert_eq!(answers["Quel langage ?"], "Go");
    }

    #[test]
    fn multiselect_is_preserved_as_array() {
        let content = BTreeMap::from([(
            question_key(0),
            ElicitationContentValue::StringArray(vec!["Rust".into(), "Go".into()]),
        )]);
        let answers = fold_answers(content, &[question(true)]);
        assert_eq!(answers["Quel langage ?"], json!(["Rust", "Go"]));
    }
}
