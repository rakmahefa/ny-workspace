//! Structured ACP elicitation bridge and Gemini `AskUserQuestion` normalization.

#![cfg(feature = "elicitation")]

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSessionScope, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ElicitationSupport {
    pub form: bool,
    pub url: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitationRequestSpec {
    pub session_id: SessionId,
    pub message: String,
    pub properties: BTreeMap<String, ElicitationPropertySchema>,
    pub required: Vec<String>,
}

impl ElicitationRequestSpec {
    pub fn new(session_id: SessionId, message: impl Into<String>) -> Self {
        Self { session_id, message: message.into(), properties: BTreeMap::new(), required: Vec::new() }
    }

    pub fn property(mut self, name: impl Into<String>, schema: ElicitationPropertySchema, required: bool) -> Self {
        let name = name.into();
        self.properties.insert(name.clone(), schema);
        if required && !self.required.contains(&name) { self.required.push(name); }
        self
    }

    fn into_acp_request(self) -> CreateElicitationRequest {
        let mut schema = ElicitationSchema::new();
        for (name, property) in self.properties {
            schema = schema.property(name, property, self.required.contains(&name));
        }
        let mode = ElicitationFormMode::new(ElicitationSessionScope::new(self.session_id), schema);
        CreateElicitationRequest::new(mode, self.message)
    }
}

pub async fn request_elicitation(cx: &ConnectionTo<Client>, request: ElicitationRequestSpec) -> Result<ElicitationOutcome, AcpError> {
    let response: CreateElicitationResponse = cx.send_request(request.into_acp_request()).block_task().await?;
    Ok(response_to_outcome(response))
}

pub async fn elicit_clarification(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message: &str,
    properties: BTreeMap<String, ElicitationPropertySchema>,
    required: Vec<String>,
) -> Result<Option<BTreeMap<String, ElicitationContentValue>>, AcpError> {
    let outcome = request_elicitation(cx, ElicitationRequestSpec {
        session_id: session_id.clone(), message: message.to_string(), properties, required,
    }).await?;
    match outcome {
        ElicitationOutcome::Accepted(content) => Ok(Some(content)),
        ElicitationOutcome::Declined | ElicitationOutcome::Cancelled => Ok(None),
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
        ElicitationAction::Accept(accept) => ElicitationOutcome::Accepted(accept.content.unwrap_or_default()),
        ElicitationAction::Decline => ElicitationOutcome::Declined,
        ElicitationAction::Cancel => ElicitationOutcome::Cancelled,
        _ => ElicitationOutcome::Cancelled,
    }
}

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

/// Représentation canonique d'une elicitation Gemini avant projection ACP.
/// Cela évite de coupler le parser Gemini au transport ACP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiElicitation {
    pub tool_name: String,
    pub questions: Vec<AskUserQuestion>,
}

impl GeminiElicitation {
    pub fn message(&self) -> String {
        if self.questions.len() == 1 {
            self.questions[0].question.clone()
        } else {
            self.questions.iter().map(|q| q.question.as_str()).collect::<Vec<_>>().join("\n")
        }
    }
}

/// Capture les variantes produites par Gemini (`AskUserQuestion`,
/// `ask_user_question`, `ask-user-question`, `elicitation`).
///
/// Le résultat est volontairement transport-independent : l'appelant peut
/// ensuite choisir `CreateElicitationRequest` si le client ACP le supporte,
/// ou un fallback interactif sinon.
pub fn capture_gemini_elicitation(tool_name: &str, input: &serde_json::Value) -> Option<GeminiElicitation> {
    let normalized = tool_name.trim().to_ascii_lowercase().replace(['-', '_'], "");
    if !matches!(normalized.as_str(), "askuserquestion" | "askuser" | "elicitation") {
        return None;
    }
    let questions = extract_ask_user_questions(input)?;
    Some(GeminiElicitation { tool_name: tool_name.to_owned(), questions })
}

pub fn extract_ask_user_questions(input: &serde_json::Value) -> Option<Vec<AskUserQuestion>> {
    let questions = input.get("questions")?.as_array()?;
    let valid: Vec<AskUserQuestion> = questions.iter()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .filter(|q: &AskUserQuestion| !q.question.trim().is_empty() && !q.options.is_empty())
        .collect();
    (!valid.is_empty()).then_some(valid)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskUserQuestionOutcome {
    Answered(serde_json::Value),
    Cancelled,
}

fn question_key(index: usize) -> String { format!("question_{index}") }
fn custom_answer_key(index: usize) -> String { format!("question_{index}_custom") }

pub fn apply_ask_user_response(
    response: &CreateElicitationResponse,
    tool_input: &serde_json::Value,
    questions: &[AskUserQuestion],
) -> AskUserQuestionOutcome {
    match &response.action {
        ElicitationAction::Decline => {
            let mut updated = tool_input.clone();
            updated["answers"] = serde_json::json!({});
            AskUserQuestionOutcome::Answered(updated)
        }
        ElicitationAction::Accept(accept) => {
            let content = accept.content.clone().unwrap_or_default();
            let mut answers = serde_json::Map::new();
            for (index, question) in questions.iter().enumerate() {
                if let Some(ElicitationContentValue::String(custom)) = content.get(&custom_answer_key(index)) {
                    let custom = custom.trim();
                    if !custom.is_empty() {
                        answers.insert(question.question.clone(), serde_json::Value::String(custom.to_string()));
                        continue;
                    }
                }
                if let Some(value) = content.get(&question_key(index)) {
                    answers.insert(question.question.clone(), elicitation_value_to_json(value));
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

pub fn is_vague_prompt(message: &str) -> bool {
    let lower = message.to_lowercase();
    let vague_keywords = ["refactor", "optimise", "optimize", "teste", "test ", "améliore", "ameliore", "simplifie", "nettoie", "réécris", "reecris"];
    vague_keywords.iter().any(|k| lower.contains(k)) && message.chars().count() < 80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(multi_select: bool) -> AskUserQuestion {
        AskUserQuestion {
            question: "Quel langage ?".into(), header: Some("Langage".into()), multi_select,
            options: vec![
                AskUserOption { label: "Rust".into(), description: Some("Sûr et rapide".into()), preview: Some("fn main() {}".into()) },
                AskUserOption { label: "Python".into(), description: Some("Simple pour prototyper".into()), preview: None },
            ],
        }
    }

    #[test]
    fn captures_gemini_ask_user_question() {
        let input = serde_json::json!({"questions":[serde_json::to_value(question(false)).unwrap()]});
        let result = capture_gemini_elicitation("AskUserQuestion", &input).expect("elicitation");
        assert_eq!(result.questions.len(), 1);
        assert_eq!(result.message(), "Quel langage ?");
    }

    #[test]
    fn captures_snake_case_variant() {
        let input = serde_json::json!({"questions":[serde_json::to_value(question(false)).unwrap()]});
        assert!(capture_gemini_elicitation("ask_user_question", &input).is_some());
    }

    #[test]
    fn ignores_regular_tool() {
        assert!(capture_gemini_elicitation("file_read", &serde_json::json!({"questions":[]})).is_none());
    }

    #[test]
    fn custom_answer_has_priority() {
        let input = serde_json::json!({"questions": []});
        let questions = vec![question(false)];
        let content = BTreeMap::from([(custom_answer_key(0), ElicitationContentValue::String("  Go  ".into()))]);
        let response = CreateElicitationResponse::accept(content);
        match apply_ask_user_response(&response, &input, &questions) {
            AskUserQuestionOutcome::Answered(updated) => assert_eq!(updated["answers"]["Quel langage ?"], "Go"),
            AskUserQuestionOutcome::Cancelled => panic!("expected answer"),
        }
    }

    #[test]
    fn decline_produces_empty_answers() {
        let input = serde_json::json!({"questions": []});
        let response = CreateElicitationResponse::decline();
        match apply_ask_user_response(&response, &input, &[question(false)]) {
            AskUserQuestionOutcome::Answered(updated) => assert_eq!(updated["answers"], serde_json::json!({})),
            AskUserQuestionOutcome::Cancelled => panic!("expected decline to be handled"),
        }
    }
}
