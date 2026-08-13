//! Builtin FollowUp tool.
//!
//! FollowUp is an agent-authored next-step action. It has no side effects and
//! is rendered through the normal ACP ToolCall/tool_ux pipeline.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolDef, ToolResult};

fn follow_up_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "label": {
                "type": "string",
                "minLength": 1,
                "description": "Short human-facing label for the suggested next action."
            },
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "The exact user-facing prompt/action that will be sent if selected."
            }
        },
        "required": ["label", "query"]
    })
}

fn follow_up_def() -> ToolDef {
    ToolDef {
        name: "FollowUp",
        description: "Offer one explicit next-step action to the user without executing it.",
        parameters_fn: follow_up_params,
    }
}

pub struct FollowUpTool;

#[async_trait::async_trait]
impl Tool for FollowUpTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(follow_up_def)
    }

    async fn execute(&self, args: &Value, _cwd: &Path, _allowed_dirs: &[PathBuf]) -> ToolResult {
        let label = match args.get("label").and_then(Value::as_str) {
            Some(value) if !value.trim().is_empty() => value.trim(),
            _ => return ToolResult::Err("FollowUp: parameter 'label' is required and must not be empty".into()),
        };
        let query = match args.get("query").and_then(Value::as_str) {
            Some(value) if !value.trim().is_empty() => value.trim(),
            _ => return ToolResult::Err("FollowUp: parameter 'query' is required and must not be empty".into()),
        };

        ToolResult::Ok(json!({"label": label, "query": query}).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_structured_follow_up_payload() {
        let result = FollowUpTool
            .execute(
                &json!({"label":"Initialiser un projet","query":"Initialisons un nouveau projet dans cet espace de travail."}),
                Path::new("/tmp"),
                &[],
            )
            .await;
        assert!(matches!(result, ToolResult::Ok(payload) if payload.contains("Initialiser un projet") && payload.contains("Initialisons un nouveau projet")));
    }

    #[tokio::test]
    async fn rejects_empty_values() {
        let result = FollowUpTool.execute(&json!({"label":"","query":"x"}), Path::new("/tmp"), &[]).await;
        assert!(matches!(result, ToolResult::Err(error) if error.contains("label")));
    }
}
