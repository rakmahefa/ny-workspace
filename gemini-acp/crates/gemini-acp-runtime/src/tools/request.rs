//! Domain model for a tool call request and its lifecycle.
//!
//! `ToolCallRequest` is the single state-bearing representation shared by
//! tool execution and interactive elicitation flows. The state machine is
//! intentionally stricter than the ACP wire protocol so invalid transitions
//! are rejected before they can leak into the transport layer.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// The semantic kind of request represented by a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallKind {
    Tool,
    Elicitation,
}

/// The internal lifecycle state of a tool call request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    Pending,
    WaitingForUser,
    Executing,
    Completed,
    Failed,
    Cancelled,
}

impl ToolCallState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ToolCallRequestError {
    #[error("invalid tool call state transition: {from:?} -> {to:?}")]
    InvalidTransition { from: ToolCallState, to: ToolCallState },
    #[error("tool call request is already terminal: {0:?}")]
    AlreadyTerminal(ToolCallState),
}

/// A normalized request that can represent both executable tools and user
/// elicitation requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub kind: ToolCallKind,
    pub state: ToolCallState,
}

impl ToolCallRequest {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
        kind: ToolCallKind,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
            kind,
            state: ToolCallState::Pending,
        }
    }

    pub fn tool(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self::new(id, name, arguments, ToolCallKind::Tool)
    }

    pub fn elicitation(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self::new(id, name, arguments, ToolCallKind::Elicitation)
    }

    pub const fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    pub fn transition(&mut self, next: ToolCallState) -> Result<(), ToolCallRequestError> {
        if self.state.is_terminal() {
            return Err(ToolCallRequestError::AlreadyTerminal(self.state));
        }

        let allowed = matches!(
            (self.state, next),
            (ToolCallState::Pending, ToolCallState::WaitingForUser)
                | (ToolCallState::Pending, ToolCallState::Executing)
                | (ToolCallState::Pending, ToolCallState::Cancelled)
                | (ToolCallState::WaitingForUser, ToolCallState::Executing)
                | (ToolCallState::WaitingForUser, ToolCallState::Failed)
                | (ToolCallState::WaitingForUser, ToolCallState::Cancelled)
                | (ToolCallState::Executing, ToolCallState::Completed)
                | (ToolCallState::Executing, ToolCallState::Failed)
                | (ToolCallState::Executing, ToolCallState::Cancelled)
        );

        if !allowed {
            return Err(ToolCallRequestError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }

        self.state = next;
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), ToolCallRequestError> {
        self.transition(ToolCallState::Cancelled)
    }
}

impl Default for ToolCallState {
    fn default() -> Self {
        Self::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_request_defaults_to_pending() {
        let request = ToolCallRequest::tool("call-1", "shell_exec", json!({"command": "pwd"}));
        assert_eq!(request.id, "call-1");
        assert_eq!(request.name, "shell_exec");
        assert_eq!(request.kind, ToolCallKind::Tool);
        assert_eq!(request.state, ToolCallState::Pending);
        assert!(!request.is_terminal());
    }

    #[test]
    fn elicitation_can_wait_for_user_before_execution() {
        let mut request = ToolCallRequest::elicitation("ask-1", "AskUserQuestion", json!({"question": "Continue?"}));
        request.transition(ToolCallState::WaitingForUser).unwrap();
        request.transition(ToolCallState::Executing).unwrap();
        request.transition(ToolCallState::Completed).unwrap();
        assert!(request.is_terminal());
    }

    #[test]
    fn invalid_backtracking_is_rejected() {
        let mut request = ToolCallRequest::tool("call-2", "file_read", json!({}));
        request.transition(ToolCallState::Executing).unwrap();
        assert!(matches!(
            request.transition(ToolCallState::Pending),
            Err(ToolCallRequestError::InvalidTransition {
                from: ToolCallState::Executing,
                to: ToolCallState::Pending
            })
        ));
    }

    #[test]
    fn terminal_requests_cannot_transition() {
        let mut request = ToolCallRequest::tool("call-3", "file_write", json!({}));
        request.cancel().unwrap();
        assert_eq!(request.state, ToolCallState::Cancelled);
        assert!(matches!(
            request.transition(ToolCallState::Executing),
            Err(ToolCallRequestError::AlreadyTerminal(ToolCallState::Cancelled))
        ));
    }

    #[test]
    fn state_and_kind_serialize_as_snake_case() {
        let request = ToolCallRequest::elicitation("ask-2", "AskUserQuestion", json!({}));
        let encoded = serde_json::to_value(request).unwrap();
        assert_eq!(encoded["kind"], "elicitation");
        assert_eq!(encoded["state"], "pending");
    }
}
