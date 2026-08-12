//! Structured ACP elicitation bridge inspired by
//! `agentclientprotocol/claude-agent-acp/src/elicitation.ts`.

#![cfg(feature = "elicitation")]

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CompleteElicitationNotification, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAction, ElicitationCapabilities as AcpElicitationCapabilities,
    ElicitationContentValue, ElicitationFormMode, ElicitationId, ElicitationPropertySchema,
    ElicitationSchema, ElicitationUrlMode, ElicitationSessionScope, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde::{Deserialize, Serialize};

pub use gemini_acp_runtime::ElicitationSupport;

pub fn support_from_client_capabilities(capabilities: Option<&AcpElicitationCapabilities>) -> ElicitationSupport {
    ElicitationSupport {
        form: capabilities.is_some_and(AcpElicitationCapabilities::supports_form),
        url: capabilities.is_some_and(AcpElicitationCapabilities::supports_url),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitationRequestSpec {
    pub session_id: SessionId,
    pub message: String,
    pub properties: BTreeMap<String, ElicitationPropertySchema>,
    pub required: Vec<String>,
}

impl ElicitationRequestSpec {
    pub fn new(session_id: SessionId, message: impl Into<String>) -> Self { Self { session_id, message: message.into(), properties: BTreeMap::new(), required: Vec::new() } }
    pub fn property(mut self, name: impl Into<String>, schema: ElicitationPropertySchema, required: bool) -> Self { let name = name.into(); self.properties.insert(name.clone(), schema); if required && !self.required.contains(&name) { self.required.push(name); } self }
    fn into_acp_request(self) -> CreateElicitationRequest { let mut schema = ElicitationSchema::new(); for (name, property) in self.properties { schema = schema.property(name, property, self.required.contains(&name)); } CreateElicitationRequest::new(ElicitationFormMode::new(ElicitationSessionScope::new(self.session_id), schema), self.message) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationError { UnsupportedMode(&'static str), InvalidUrl(String), Transport(String) }
impl std::fmt::Display for ElicitationError { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { match self { Self::UnsupportedMode(mode) => write!(f, "client does not support {mode} elicitation"), Self::InvalidUrl(url) => write!(f, "invalid elicitation URL: {url}"), Self::Transport(error) => write!(f, "ACP elicitation transport failed: {error}") } } }
impl std::error::Error for ElicitationError {}

pub async fn request_elicitation(cx: &ConnectionTo<Client>, support: ElicitationSupport, request: ElicitationRequestSpec) -> Result<ElicitationOutcome, ElicitationError> { if !support.form { return Err(ElicitationError::UnsupportedMode("form")); } let response = cx.send_request(request.into_acp_request()).block_task().await.map_err(|e| ElicitationError::Transport(e.to_string()))?; Ok(response_to_outcome(response)) }

pub async fn request_url_elicitation(cx: &ConnectionTo<Client>, support: ElicitationSupport, session_id: &SessionId, message: &str, elicitation_id: impl Into<ElicitationId>, url: impl Into<String>) -> Result<UrlElicitationOutcome, ElicitationError> { if !support.url { return Err(ElicitationError::UnsupportedMode("URL")); } let url = url.into(); if !(url.starts_with("https://") || url.starts_with("http://")) { return Err(ElicitationError::InvalidUrl(url)); } let request = CreateElicitationRequest::new(ElicitationUrlMode::new(ElicitationSessionScope::new(session_id.clone()), elicitation_id, url), message); let response = cx.send_request(request).block_task().await.map_err(|e| ElicitationError::Transport(e.to_string()))?; Ok(match response_to_outcome(response) { ElicitationOutcome::Accepted(c) => UrlElicitationOutcome::Accepted(c), ElicitationOutcome::Declined => UrlElicitationOutcome::Declined, ElicitationOutcome::Cancelled => UrlElicitationOutcome::Cancelled }) }

pub fn complete_url_elicitation(cx: &ConnectionTo<Client>, elicitation_id: impl Into<ElicitationId>) { let _ = cx.send_notification(CompleteElicitationNotification::new(elicitation_id)); }

pub async fn elicit_clarification(cx: &ConnectionTo<Client>, support: ElicitationSupport, session_id: &SessionId, message: &str, properties: BTreeMap<String, ElicitationPropertySchema>, required: Vec<String>) -> Result<Option<BTreeMap<String, ElicitationContentValue>>, ElicitationError> { match request_elicitation(cx, support, ElicitationRequestSpec { session_id: session_id.clone(), message: message.to_string(), properties, required }).await? { ElicitationOutcome::Accepted(content) => Ok(Some(content)), ElicitationOutcome::Declined | ElicitationOutcome::Cancelled => Ok(None) } }

#[derive(Debug, Clone, PartialEq)] pub enum ElicitationOutcome { Accepted(BTreeMap<String, ElicitationContentValue>), Declined, Cancelled }
#[derive(Debug, Clone, PartialEq)] pub enum UrlElicitationOutcome { Accepted(BTreeMap<String, ElicitationContentValue>), Declined, Cancelled }

pub fn response_to_outcome(response: CreateElicitationResponse) -> ElicitationOutcome { match response.action { ElicitationAction::Accept(accept) => ElicitationOutcome::Accepted(accept.content.unwrap_or_default()), ElicitationAction::Decline => ElicitationOutcome::Declined, ElicitationAction::Cancel => ElicitationOutcome::Cancelled, _ => ElicitationOutcome::Cancelled } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] pub struct AskUserQuestion { pub question: String, #[serde(default)] pub header: Option<String>, pub options: Vec<AskUserOption>, #[serde(default)] pub multi_select: bool }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] pub struct AskUserOption { pub label: String, #[serde(default)] pub description: Option<String>, #[serde(default)] pub preview: Option<String> }
pub fn extract_ask_user_questions(input: &serde_json::Value) -> Option<Vec<AskUserQuestion>> { let questions = input.get("questions")?.as_array()?; let valid: Vec<AskUserQuestion> = questions.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).filter(|q: &AskUserQuestion| !q.question.trim().is_empty() && !q.options.is_empty()).collect(); (!valid.is_empty()).then_some(valid) }
#[derive(Debug, Clone, PartialEq, Eq)] pub enum AskUserQuestionOutcome { Answered(serde_json::Value), Cancelled }
fn question_key(index: usize) -> String { format!("question_{index}") }
fn custom_answer_key(index: usize) -> String { format!("question_{index}_custom") }
pub fn apply_ask_user_response(response: &CreateElicitationResponse, tool_input: &serde_json::Value, questions: &[AskUserQuestion]) -> AskUserQuestionOutcome { match &response.action { ElicitationAction::Decline => { let mut updated = tool_input.clone(); updated["answers"] = serde_json::json!({}); AskUserQuestionOutcome::Answered(updated) }, ElicitationAction::Accept(accept) => { let content = accept.content.clone().unwrap_or_default(); let mut answers = serde_json::Map::new(); for (index, question) in questions.iter().enumerate() { if let Some(ElicitationContentValue::String(custom)) = content.get(&custom_answer_key(index)) { let custom = custom.trim(); if !custom.is_empty() { answers.insert(question.question.clone(), serde_json::Value::String(custom.to_string())); continue; } } if let Some(value) = content.get(&question_key(index)) { answers.insert(question.question.clone(), elicitation_value_to_json(value)); } } let mut updated = tool_input.clone(); updated["answers"] = serde_json::Value::Object(answers); AskUserQuestionOutcome::Answered(updated) }, ElicitationAction::Cancel | _ => AskUserQuestionOutcome::Cancelled } }
fn elicitation_value_to_json(value: &ElicitationContentValue) -> serde_json::Value { match value { ElicitationContentValue::String(v) => serde_json::Value::String(v.clone()), ElicitationContentValue::Boolean(v) => serde_json::Value::Bool(*v), ElicitationContentValue::Number(v) => serde_json::json!(v), ElicitationContentValue::StringArray(v) => serde_json::json!(v), _ => serde_json::Value::Null } }
pub fn is_vague_prompt(message: &str) -> bool { let lower = message.to_lowercase(); let vague_keywords = ["refactor", "optimise", "optimize", "teste", "test ", "améliore", "ameliore", "simplifie", "nettoie", "réécris", "reecris"]; vague_keywords.iter().any(|k| lower.contains(k)) && message.chars().count() < 80 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn capability_mapping_is_exact() { let caps = AcpElicitationCapabilities::new(); let support = support_from_client_capabilities(Some(&caps)); assert!(!support.form && !support.url); }
    #[test] fn capability_mapping_with_form() { let caps = serde_json::from_value::<AcpElicitationCapabilities>(serde_json::json!({"form": {}})).expect("caps"); let support = support_from_client_capabilities(Some(&caps)); assert!(support.form); assert!(!support.url); }
    #[test] fn missing_capability_means_no_support() { let support = support_from_client_capabilities(None); assert!(!support.form && !support.url); }
    #[test] fn vague_prompt_heuristic_remains_stable() { assert!(is_vague_prompt("Refactor ce fichier")); assert!(!is_vague_prompt("Explique-moi le pattern MVP en Rust")); }
}
