//! ACP-backed interactive tools.
//!
//! The interactive tool only owns task-local session context and delegates the
//! protocol projection to the shared `elicitation` bridge. This keeps tool
//! orchestration separate from ACP schema/response handling.

use std::future::Future;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::{Client, ConnectionTo};
use serde_json::{json, Value};

use super::elicitation::{parse_questions, request_user_input};
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
        let questions = match parse_questions(args) {
            Ok(questions) => questions,
            Err(error) => return ToolResult::Err(error),
        };

        let Some(context) = current_context() else {
            return ToolResult::Err(
                "AskUserQuestion is unavailable outside an ACP prompt turn.".to_string(),
            );
        };

        match request_user_input(&context.cx, &context.session_id, &questions).await {
            Ok(value) => ToolResult::Ok(value),
            Err(error) => ToolResult::Err(error),
        }
    }
}
