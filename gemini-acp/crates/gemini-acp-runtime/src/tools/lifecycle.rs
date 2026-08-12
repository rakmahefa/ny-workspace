//! Deterministic tool-call lifecycle and session cancellation bridge.
//!
//! ACP v1 only exposes Pending/InProgress/Completed/Failed on the wire.
//! We therefore keep `Permission` and `Cancelled` as internal states and
//! project them onto the stable ACP statuses while carrying the semantic
//! reason in `_meta`.

use std::collections::HashMap;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, OnceLock};

use agent_client_protocol::schema::v1::ToolCallStatus;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLifecycleState { Pending, Permission, Executing, Completed, Failed, Cancelled }

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
    InvalidTransition { from: ToolLifecycleState, to: ToolLifecycleState },
    #[error("tool lifecycle is already terminal: {0:?}")]
    AlreadyTerminal(ToolLifecycleState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLifecycle { state: ToolLifecycleState, sequence: u64 }

impl Default for ToolLifecycle { fn default() -> Self { Self::new() } }

impl ToolLifecycle {
    pub const fn new() -> Self { Self { state: ToolLifecycleState::Pending, sequence: 0 } }
    pub const fn state(&self) -> ToolLifecycleState { self.state }
    pub const fn sequence(&self) -> u64 { self.sequence }
    pub fn transition(&mut self, next: ToolLifecycleState) -> Result<(), LifecycleError> {
        if self.state.is_terminal() { return Err(LifecycleError::AlreadyTerminal(self.state)); }
        let allowed = matches!((self.state, next),
            (ToolLifecycleState::Pending, ToolLifecycleState::Permission)
            | (ToolLifecycleState::Pending, ToolLifecycleState::Executing)
            | (ToolLifecycleState::Permission, ToolLifecycleState::Executing)
            | (ToolLifecycleState::Permission, ToolLifecycleState::Failed)
            | (ToolLifecycleState::Permission, ToolLifecycleState::Cancelled)
            | (ToolLifecycleState::Executing, ToolLifecycleState::Completed)
            | (ToolLifecycleState::Executing, ToolLifecycleState::Failed)
            | (ToolLifecycleState::Executing, ToolLifecycleState::Cancelled));
        if !allowed { return Err(LifecycleError::InvalidTransition { from: self.state, to: next }); }
        self.state = next;
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }
    pub fn cancel(&mut self) -> Result<(), LifecycleError> { self.transition(ToolLifecycleState::Cancelled) }
}

// Bridge for session/cancel: Store remains the authoritative turn guard,
// while the runtime/tool layer can observe cancellation without depending on
// the agent crate.
type SessionCancellationMap = HashMap<String, Arc<AtomicBool>>;
static SESSION_CANCELLATION: OnceLock<Mutex<SessionCancellationMap>> = OnceLock::new();

fn cancellation_map() -> &'static Mutex<SessionCancellationMap> {
    SESSION_CANCELLATION.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn reset_session_cancellation(session_id: &str) {
    let flag = {
        let mut map = cancellation_map().lock().expect("session cancellation mutex poisoned");
        map.entry(session_id.to_owned()).or_insert_with(|| Arc::new(AtomicBool::new(false))).clone()
    };
    flag.store(false, Ordering::Release);
}

pub fn cancel_session(session_id: &str) {
    let flag = {
        let mut map = cancellation_map().lock().expect("session cancellation mutex poisoned");
        map.entry(session_id.to_owned()).or_insert_with(|| Arc::new(AtomicBool::new(false))).clone()
    };
    flag.store(true, Ordering::Release);
}

pub fn session_cancelled(session_id: &str) -> bool {
    let map = cancellation_map().lock().expect("session cancellation mutex poisoned");
    map.get(session_id).map(|flag| flag.load(Ordering::Acquire)).unwrap_or(false)
}

pub async fn wait_for_session_cancel(session_id: &str) {
    loop {
        if session_cancelled(session_id) { return; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_is_strict() {
        let mut lifecycle = ToolLifecycle::new();
        lifecycle.transition(ToolLifecycleState::Permission).unwrap();
        lifecycle.transition(ToolLifecycleState::Executing).unwrap();
        lifecycle.transition(ToolLifecycleState::Completed).unwrap();
        assert_eq!(lifecycle.sequence(), 3);
        assert_eq!(lifecycle.state().wire_status(), ToolCallStatus::Completed);
    }
    #[test]
    fn cancellation_is_wire_compatible() {
        let mut lifecycle = ToolLifecycle::new();
        lifecycle.transition(ToolLifecycleState::Permission).unwrap();
        lifecycle.cancel().unwrap();
        assert_eq!(lifecycle.state().wire_status(), ToolCallStatus::Failed);
    }
    #[test]
    fn illegal_backtracking_is_rejected() {
        let mut lifecycle = ToolLifecycle::new();
        lifecycle.transition(ToolLifecycleState::Executing).unwrap();
        assert!(matches!(lifecycle.transition(ToolLifecycleState::Pending), Err(LifecycleError::InvalidTransition { .. })));
        lifecycle.transition(ToolLifecycleState::Failed).unwrap();
        assert!(matches!(lifecycle.transition(ToolLifecycleState::Completed), Err(LifecycleError::AlreadyTerminal(ToolLifecycleState::Failed))));
    }
    #[test]
    fn session_cancellation_is_resettable() {
        reset_session_cancellation("sess-test");
        assert!(!session_cancelled("sess-test"));
        cancel_session("sess-test");
        assert!(session_cancelled("sess-test"));
        reset_session_cancellation("sess-test");
        assert!(!session_cancelled("sess-test"));
    }
}
