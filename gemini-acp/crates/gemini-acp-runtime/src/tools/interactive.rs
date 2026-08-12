//! ACP-backed interactive tools.
//!
//! `AskUserQuestion` is a builtin tool from the agent's point of view, but its
//! implementation lives on the client side of ACP: the tool pauses the current
//! turn, asks the connected client for structured input, then returns the
//! selected answers as a normal tool result. A Tokio task-local context keeps
//! concurrent sessions isolated without putting a client connection into the
//! shared `ToolRegistry`.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, ElicitationAction, ElicitationContentValue, ElicitationFormMode,
    ElicitationPropertySchema, ElicitationSchema, ElicitationSessionScope, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde::Deserialize;
use serde_json::{json, Value};

use super::registry::{Tool, ToolDef, ToolResult};

#[derive(Clone)]
pub struct InteractiveContext {
    pub cx: ConnectionTo<Client>,
    pub session_id: SessionId,
}

tokio::task_local! {
    static CONTEXT: InteractiveContext;
}

pub async fn scope<F>(context: InteractiveContext, future: F) -> F::Output
where
    F: Future,
{
    CONTEXT.scope(context, future).await
}

fn current_context() -> Option<InteractiveContext> {
    CONTEXT.try_with(|context| context.clone()).ok()
}

#[derive(Debug, Clone, Deserialize)]
struct AskUserInput {
    questions: Vec<AskUserQuestion>,
}

#[derive(Debug, Clone, Deserialize)]
struct AskUserQuestion {
    question: String,
    #[serde(default)]
    header: Option<String>,
    options: Vec<AskUserOption>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct AskUserOption {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    preview: Option<String>,
}

pub struct AskUserQuestionTool;

fn definition() -> ToolDef {
    ToolDef {
        name: "AskUserQuestion",
        description: "Ask the user one or more structured questions and wait for their answers.",
        parameters_fn: || {
            json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "Questions to ask the user.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string" },
                                "header": { "type": "string" },
                                "multi_select": { "type": "boolean" },
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string" },
                                            "description": { "type": "string" },
                                            "preview": { "type": "string" }
                                        },
                                        "required": ["label"]
                                    }
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            })
        },
    }
}

#[async_trait::async_trait]
impl Tool for AskUserQuestionTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(definition)
    }

    async fn execute(&self, args: &Value, _cwd: &Path, _allowed_dirs: &[PathBuf]) -> ToolResult {
        let input: AskUserInput = match serde_json::from_value(args.clone()) {
            Ok(input) => input,
            Err(error) => return ToolResult::Err(format!("invalid AskUserQuestion input: {error}")),
        };

        if input.questions.is_empty() {
            return ToolResult::Err("AskUserQuestion requires at least one question.".to_string());
        }

        let Some(context) = current_context() else {
            return ToolResult::Err("AskUserQuestion is unavailable outside an ACP prompt turn.".to_string());
        };

        match request_user_input(&context.cx, &context.session_id, input.questions).await {
            Ok(value) => ToolResult::Ok(value),
            Err(error) => ToolResult::Err(error),
        }
    }
}

async fn request_user_input(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    questions: Vec<AskUserQuestion>,
) -> Result<String, String> {
    let properties = build_question_properties(&questions)?;
    let mut schema = ElicitationSchema::new();
    for (name, property) in properties {
        schema = schema.property(name, property, false);
    }

    let mode = ElicitationFormMode::new(ElicitationSessionScope::new(session_id.clone()), schema);
    let request = CreateElicitationRequest::new(
        mode,
        if questions.len() == 1 {
            questions[0].question.clone()
        } else {
            "Please answer the following questions.".to_string()
        },
    );

    let response = cx
        .send_request(request)
        .block_task()
        .await
        .map_err(|error| format!("ACP elicitation request failed: {error}"))?;

    match response.action {
        ElicitationAction::Accept(accept) => {
            let answers = fold_answers(accept.content.unwrap_or_default(), &questions);
            Ok(json!({ "answers": answers }).to_string())
        }
        ElicitationAction::Decline => Ok(json!({ "answers": {} }).to_string()),
        ElicitationAction::Cancel => Err("user cancelled AskUserQuestion".to_string()),
        _ => Err("ACP returned an unsupported elicitation action".to_string()),
    }
}

fn build_question_properties(
    questions: &[AskUserQuestion],
) -> Result<BTreeMap<String, ElicitationPropertySchema>, String> {
    let mut properties = BTreeMap::new();

    for (index, question) in questions.iter().enumerate() {
        if question.question.trim().is_empty() || question.options.is_empty() {
            return Err("AskUserQuestion contains an empty question or no options".to_string());
        }

        let options: Vec<Value> = question
            .options
            .iter()
            .map(|option| {
                let mut value = json!({
                    "const": option.label,
                    "title": option.label,
                });
                if let Some(description) = &option.description {
                    value["description"] = Value::String(description.clone());
                }
                if let Some(preview) = &option.preview {
                    value["_meta"] = json!({
                        "_claude/askUserQuestionOption": { "preview": preview }
                    });
                }
                value
            })
            .collect();

        let description = if questions.len() == 1 {
            None
        } else {
            Some(question.question.clone())
        };

        let property_json = if question.multi_select {
            json!({
                "type": "array",
                "title": question.header,
                "description": description,
                "items": { "anyOf": options }
            })
        } else {
            json!({
                "type": "string",
                "title": question.header,
                "description": description,
                "oneOf": options
            })
        };

        let property: ElicitationPropertySchema = serde_json::from_value(property_json)
            .map_err(|error| format!("invalid ACP elicitation schema for question {index}: {error}"))?;
        properties.insert(format!("question_{index}"), property);

        let custom: ElicitationPropertySchema = serde_json::from_value(json!({
            "type": "string",
            "title": "Other",
            "description": "Type your own answer instead of choosing an option above (optional).",
            "_meta": {
                "_askUserQuestionCustomAnswer": {
                    "questionId": format!("question_{index}")
                }
            }
        }))
        .map_err(|error| format!("invalid ACP custom-answer schema: {error}"))?;
        properties.insert(format!("question_{index}_custom"), custom);
    }

    Ok(properties)
}

fn fold_answers(
    content: BTreeMap<String, ElicitationContentValue>,
    questions: &[AskUserQuestion],
) -> serde_json::Map<String, Value> {
    let mut answers = serde_json::Map::new();

    for (index, question) in questions.iter().enumerate() {
        let custom_key = format!("question_{index}_custom");
        if let Some(ElicitationContentValue::String(custom)) = content.get(&custom_key) {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                answers.insert(question.question.clone(), Value::String(trimmed.to_string()));
                continue;
            }
        }

        let key = format!("question_{index}");
        if let Some(value) = content.get(&key) {
            answers.insert(question.question.clone(), content_value_to_json(value));
        }
    }

    answers
}

fn content_value_to_json(value: &ElicitationContentValue) -> Value {
    match value {
        ElicitationContentValue::String(value) => Value::String(value.clone()),
        ElicitationContentValue::Boolean(value) => Value::Bool(*value),
        ElicitationContentValue::Number(value) => json!(value),
        ElicitationContentValue::StringArray(values) => json!(values),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> Vec<AskUserQuestion> {
        vec![AskUserQuestion {
            question: "Quel langage ?".to_string(),
            header: Some("Langage".to_string()),
            multi_select: false,
            options: vec![
                AskUserOption {
                    label: "Rust".to_string(),
                    description: Some("Sûr et rapide".to_string()),
                    preview: Some("fn main() {}".to_string()),
                },
                AskUserOption {
                    label: "Python".to_string(),
                    description: None,
                    preview: None,
                },
            ],
        }]
    }

    #[test]
    fn builds_typed_form_properties() {
        let properties = build_question_properties(&questions()).expect("schema");
        assert_eq!(properties.len(), 2);
    }

    #[test]
    fn custom_answer_takes_precedence() {
        let content = BTreeMap::from([
            (
                "question_0".to_string(),
                ElicitationContentValue::String("Rust".to_string()),
            ),
            (
                "question_0_custom".to_string(),
                ElicitationContentValue::String("  Go  ".to_string()),
            ),
        ]);
        let answers = fold_answers(content, &questions());
        assert_eq!(answers["Quel langage ?"], "Go");
    }

    #[test]
    fn decline_is_serialized_as_empty_answers() {
        let value = json!({"answers": {}}).to_string();
        assert_eq!(value, "{\"answers\":{}}");
    }
}
