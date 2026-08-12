//! Deterministic tool-call lifecycle.
//!
//! ACP v1 only exposes Pending/InProgress/Completed/Failed on the wire.
//! We therefore keep `Permission` and `Cancelled` as internal states and
//! project them onto the stable ACP statuses while carrying the semantic
//! reason in `_meta`.

use agent_client_protocol::schema::v1::ToolCallStatus;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLifecycleState {
    Pending,
    Permission,
    Executing,
    Completed,
    Failed,
    Cancelled,
}

impl ToolLifecycleState {
    pub const fn wire_status(self) -> ToolCallStatus {
        match self {
            Self::Pending | Self::Permission => ToolCallStatus::Pending,
            Self::Executing => ToolCallStatus::InProgress,
            Self::Completed => ToolCallStatus::Completed,
            Self::Failed | Self::Cancelled => ToolCallStatus::Failed,
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    #[error("invalid tool lifecycle transition: {from:?} -> {to:?}")]
    InvalidTransition {
        from: ToolLifecycleState,
        to: ToolLifecycleState,
    },
    #[error("tool lifecycle is already terminal: {0:?}")]
    AlreadyTerminal(ToolLifecycleState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLifecycle {
    state: ToolLifecycleState,
    sequence: u64,
}

impl Default for ToolLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolLifecycle {
    pub const fn new() -> Self {
        Self {
            state: ToolLifecycleState::Pending,
            sequence: 0,
        }
    }

    pub const fn state(&self) -> ToolLifecycleState {
        self.state
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn transition(&mut self, next: ToolLifecycleState) -> Result<(), LifecycleError> {
        if self.state.is_terminal() {
            return Err(LifecycleError::AlreadyTerminal(self.state));
        }

        let allowed = matches!(
            (self.state, next),
            (ToolLifecycleState::Pending, ToolLifecycleState::Permission)
                | (ToolLifecycleState::Pending, ToolLifecycleState::Executing)
                | (ToolLifecycleState::Permission, ToolLifecycleState::Executing)
                | (ToolLifecycleState::Permission, ToolLifecycleState::Failed)
                | (ToolLifecycleState::Permission, ToolLifecycleState::Cancelled)
                | (ToolLifecycleState::Executing, ToolLifecycleState::Completed)
                | (ToolLifecycleState::Executing, ToolLifecycleState::Failed)
                | (ToolLifecycleState::Executing, ToolLifecycleState::Cancelled)
        );

        if !allowed {
            return Err(LifecycleError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }

        self.state = next;
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), LifecycleError> {
        self.transition(ToolLifecycleState::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_is_strict_and_deterministic() {
        let mut lifecycle = ToolLifecycle::new();
        assert_eq!(lifecycle.state(), ToolLifecycleState::Pending);
        assert_eq!(lifecycle.sequence(), 0);

        lifecycle.transition(ToolLifecycleState::Permission).unwrap();
        lifecycle.transition(ToolLifecycleState::Executing).unwrap();
        lifecycle.transition(ToolLifecycleState::Completed).unwrap();

        assert_eq!(lifecycle.state(), ToolLifecycleState::Completed);
        assert_eq!(lifecycle.sequence(), 3);
        assert!(lifecycle.state().is_terminal());
        assert_eq!(lifecycle.state().wire_status(), ToolCallStatus::Completed);
    }

    #[test]
    fn no_permission_path_skips_permission_state() {
        let mut lifecycle = ToolLifecycle::new();
        lifecycle.transition(ToolLifecycleState::Executing).unwrap();
        lifecycle.transition(ToolLifecycleState::Completed).unwrap();
        assert_eq!(lifecycle.sequence(), 2);
    }

    #[test]
    fn rejection_and_cancellation_are_terminal_but_wire_compatible() {
        let mut rejected = ToolLifecycle::new();
        rejected.transition(ToolLifecycleState::Permission).unwrap();
        rejected.transition(ToolLifecycleState::Failed).unwrap();
        assert_eq!(rejected.state().wire_status(), ToolCallStatus::Failed);

        let mut cancelled = ToolLifecycle::new();
        cancelled.transition(ToolLifecycleState::Permission).unwrap();
        cancelled.cancel().unwrap();
        assert_eq!(cancelled.state(), ToolLifecycleState::Cancelled);
        assert_eq!(cancelled.state().wire_status(), ToolCallStatus::Failed);
    }

    #[test]
    fn illegal_backtracking_is_rejected() {
        let mut lifecycle = ToolLifecycle::new();
        lifecycle.transition(ToolLifecycleState::Executing).unwrap();
        let error = lifecycle.transition(ToolLifecycleState::Pending).unwrap_err();
        assert!(matches!(error, LifecycleError::InvalidTransition { .. }));

        lifecycle.transition(ToolLifecycleState::Failed).unwrap();
        assert!(matches!(
            lifecycle.transition(ToolLifecycleState::Completed),
            Err(LifecycleError::AlreadyTerminal(ToolLifecycleState::Failed))
        ));
    }
}
