//! Structured ACP elicitation helpers inspired by
//! `agentclientprotocol/claude-agent-acp/src/elicitation.ts`.
//!
//! The bridge keeps transport concerns small and makes the transformation
//! logic independently testable: generic form/url requests are normalized,
//! ACP responses become stable Rust outcomes, and AskUserQuestion-style input
//! can be rendered as an ACP form and folded back into tool input.

#![cfg(feature = "elicitation")]

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSessionScope, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Capabilities advertised by the connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ElicitationSupport {
    pub form: bool,
    pub url: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationMode {
    Form,
    Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitationRequestSpec {
    pub mode: ElicitationMode,
    pub session_id: SessionId,
    pub message: String,
    pub requested_schema: Option<BTreeMap<String, ElicitationPropertySchema>>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
}

/// Normalize an upstream elicitation request into ACP.
pub fn to_create_request(request: ElicitationRequestSpec) -> Option<CreateElicitationRequest> {
    match request.mode {
        ElicitationMode::Form => {
            let mut schema = ElicitationSchema::new();
            for (name, property) in request.requested_schema.unwrap_or_default() {
                schema = schema.property(name, property, false);
            }
            let mode = ElicitationFormMode::new(
                ElicitationSessionScope::new(request.session_id),
                schema,
            );
            Some(CreateElicitationRequest::new(mode, request.message))
        }
        ElicitationMode::Url => {
            let url = request.url?;
            let id = request
                .elicitation_id
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let mode = agent_client_protocol::schema::v1::ElicitationUrlMode::new(
                ElicitationSessionScope::new(request.session_id),
                url,
                id,
            );
            Some(CreateElicitationRequest::new(mode, request.message))
        }
    }
}

/// Send an elicitation request through the connected ACP client.
pub async fn request_elicitation(
    cx: &ConnectionTo<Client>,
    request: ElicitationRequestSpec,
) -> Result<Option<BTreeMap<String, ElicitationContentValue>>, AcpError> {
    let wire_request = to_create_request(request).ok_or_else(|| {
        AcpError::internal_error(
            "elicitation request cannot be represented by ACP".to_string(),
        )
    })?;

    let response: CreateElicitationResponse =
        cx.send_request(wire_request).block_task().await?;

    match response.action {
        ElicitationAction::Accept(accept) => Ok(accept.content),
        ElicitationAction::Decline | ElicitationAction::Cancel => Ok(None),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationOutcome {
    Accepted(BTreeMap<String, ElicitationContentValue>),
    Declined,
    Cancelled,
}

pub fn response_to_outcome(response: CreateElicitationResponse) -> ElicitationOutcome {
    match response.action {
        ElicitationAction::Accept(accept) => {
            ElicitationOutcome::Accepted(accept.content.unwrap_or_default())
        }
        ElicitationAction::Decline => ElicitationOutcome::Declined,
        ElicitationAction::Cancel => ElicitationOutcome::Cancelled,
        _ => ElicitationOutcome::Cancelled,
    }
}

/// Normalized AskUserQuestion-style input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskUserQuestion {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    pub options: Vec<AskUserOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskUserOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
}

pub fn extract_ask_user_questions(
    input: &serde_json::Value,
) -> Option<Vec<AskUserQuestion>> {
    let questions = input.get("questions")?.as_array()?;
    let parsed: Vec<AskUserQuestion> = questions
        .iter()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .filter(|question: &AskUserQuestion| {
            !question.question.trim().is_empty() && !question.options.is_empty()
        })
        .collect();

    (!parsed.is_empty()).then_some(parsed)
}

const OPTION_META_KEY: &str = "_claude/askUserQuestionOption";
const CUSTOM_ANSWER_META_KEY: &str = "_askUserQuestionCustomAnswer";

fn question_key(index: usize) -> String {
    format!("question_{index}")
}

fn custom_answer_key(index: usize) -> String {
    format!("question_{index}_custom")
}

/// Render AskUserQuestion-style input as an ACP form elicitation.
pub fn ask_user_questions_to_request(
    questions: &[AskUserQuestion],
    session_id: SessionId,
    tool_call_id: Option<String>,
) -> CreateElicitationRequest {
    let single = questions.len() == 1;
    let mut schema = ElicitationSchema::new();

    for (index, question) in questions.iter().enumerate() {
        let mut options = Vec::with_capacity(question.options.len());
        for option in &question.options {
            let mut enum_option = agent_client_protocol::schema::v1::EnumOption::new(
                option.label.clone(),
                option.label.clone(),
            );
            if let Some(description) = &option.description {
                enum_option = enum_option.description(description.clone());
            }
            if let Some(preview) = &option.preview {
                enum_option = enum_option.meta(serde_json::json!({
                    OPTION_META_KEY: { "preview": preview }
                }));
            }
            options.push(enum_option);
        }

        let description = (!single).then(|| question.question.clone());
        let property = if question.multi_select {
            ElicitationPropertySchema::array_of_enum(
                options,
                question.header.clone(),
                description,
            )
        } else {
            ElicitationPropertySchema::string_enum(
                options,
                question.header.clone(),
                description,
            )
        };
        schema = schema.property(question_key(index), property, false);

        let custom = ElicitationPropertySchema::string(
            Some("Other".to_string()),
            Some("Type your own answer instead of choosing an option (optional).".to_string()),
        )
        .meta(serde_json::json!({
            CUSTOM_ANSWER_META_KEY: {
                "questionId": question_key(index),
                "isCustomAnswer": true
            }
        }));
        schema = schema.property(custom_answer_key(index), custom, false);
    }

    let mut request = CreateElicitationRequest::new(
        ElicitationFormMode::new(ElicitationSessionScope::new(session_id), schema),
        if single {
            questions
                .first()
                .map(|q| q.question.clone())
                .unwrap_or_default()
        } else {
            "Please answer the following questions.".to_string()
        },
    );

    if let Some(tool_call_id) = tool_call_id {
        request = request.tool_call_id(tool_call_id);
    }

    request
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskUserQuestionOutcome {
    Answered(serde_json::Value),
    Cancelled,
}

pub fn apply_ask_user_response(
    response: &CreateElicitationResponse,
    tool_input: &serde_json::Value,
    questions: &[AskUserQuestion],
) -> AskUserQuestionOutcome {
    match response.action {
        ElicitationAction::Decline => {
            let mut updated = tool_input.clone();
            updated["answers"] = serde_json::json!({});
            AskUserQuestionOutcome::Answered(updated)
        }
        ElicitationAction::Accept(accept) => {
            let content = accept.content.clone().unwrap_or_default();
            let mut answers = serde_json::Map::new();

            for (index, question) in questions.iter().enumerate() {
                if let Some(ElicitationContentValue::String(custom)) =
                    content.get(&custom_answer_key(index))
                {
                    let custom = custom.trim();
                    if !custom.is_empty() {
                        answers.insert(
                            question.question.clone(),
                            serde_json::Value::String(custom.to_string()),
                        );
                        continue;
                    }
                }

                if let Some(value) = content.get(&question_key(index)) {
                    answers.insert(
                        question.question.clone(),
                        elicitation_value_to_json(value),
                    );
                }
            }

            let mut updated = tool_input.clone();
            updated["answers"] = serde_json::Value::Object(answers);
            AskUserQuestionOutcome::Answered(updated)
        }
        ElicitationAction::Cancel => AskUserQuestionOutcome::Cancelled,
        _ => AskUserQuestionOutcome::Cancelled,
    }
}

fn elicitation_value_to_json(value: &ElicitationContentValue) -> serde_json::Value {
    match value {
        ElicitationContentValue::String(value) => serde_json::Value::String(value.clone()),
        ElicitationContentValue::Boolean(value) => serde_json::Value::Bool(*value),
        ElicitationContentValue::Number(value) => serde_json::json!(value),
        ElicitationContentValue::StringArray(values) => serde_json::json!(values),
        _ => serde_json::Value::Null,
    }
}

/// Preserve the existing heuristic used by the earlier helper.
pub fn is_vague_prompt(message: &str) -> bool {
    let lower = message.to_lowercase();
    const KEYWORDS: &[&str] = &[
        "refactor", "optimise", "optimize", "teste", "test ", "améliore", "ameliore",
        "simplifie", "nettoie", "réécris", "reecris",
    ];
    KEYWORDS.iter().any(|keyword| lower.contains(keyword)) && message.chars().count() < 80
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
                    description: Some("Simple pour prototyper".into()),
                    preview: None,
                },
            ],
        }
    }

    #[test]
    fn extracts_valid_questions() {
        let input = serde_json::json!({
            "questions": [serde_json::to_value(question(false)).unwrap(), {"question":"", "options":[]}]
        });
        let result = extract_ask_user_questions(&input).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn vague_prompt_matches_existing_behavior() {
        assert!(is_vague_prompt("Refactor ce fichier"));
        assert!(!is_vague_prompt("Explique-moi le pattern MVP en Rust"));
    }

    #[test]
    fn custom_answer_wins_over_selection() {
        let input = serde_json::json!({"questions": []});
        let questions = vec![question(false)];
        let content = BTreeMap::from([(
            custom_answer_key(0),
            ElicitationContentValue::String("  Go  ".into()),
        )]);
        let response = CreateElicitationResponse::accept(content);
        match apply_ask_user_response(&response, &input, &questions) {
            AskUserQuestionOutcome::Answered(updated) => {
                assert_eq!(updated["answers"]["Quel langage ?"], "Go");
            }
            AskUserQuestionOutcome::Cancelled => panic!("expected accepted response"),
        }
    }
}
