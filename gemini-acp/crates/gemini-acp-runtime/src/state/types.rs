//! Types du module state : rôles, modes de session, données persistées, erreurs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_SNAPSHOTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "tool")]
    Tool,
}

/// Mode de permission de la session.
///
/// En mode `default`, les outils mutatifs déclenchent une vraie requête ACP
/// `session/request_permission` vers le client. `accept_edits` autorise les
/// écritures et conserve une confirmation pour les commandes à risque élevé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionMode {
    #[default]
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "accept_edits")]
    AcceptEdits,
    #[serde(rename = "bypass_permissions")]
    BypassPermissions,
}

impl SessionMode {
    pub fn all() -> &'static [SessionMode] {
        &[SessionMode::Default, SessionMode::AcceptEdits, SessionMode::BypassPermissions]
    }

    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "accept_edits" => Some(Self::AcceptEdits),
            "bypass_permissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Default => "Ask for permission",
            Self::AcceptEdits => "Auto-approve edits",
            Self::BypassPermissions => "Bypass all permissions",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Default => "Ask the ACP client before edits and commands.",
            Self::AcceptEdits => "Edits run without prompting. High-risk commands still require ACP permission.",
            Self::BypassPermissions => "Edits and commands run without prompting.",
        }
    }

    pub fn requires_write_permission(&self) -> bool { matches!(self, Self::Default) }
    pub fn requires_execute_permission(&self) -> bool { !matches!(self, Self::BypassPermissions) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub model: String,
    pub think: Option<u32>,
    pub tools_enabled: bool,
    #[serde(default)]
    pub mode: SessionMode,
    #[serde(default)]
    pub turn_count: u64,
    pub messages: Vec<(Role, String)>,
}

impl Session {
    pub fn new(id: String, cwd: PathBuf, additional_directories: Vec<PathBuf>, model: &str) -> Self {
        let now = gemini_acp_config::core::time::now_iso();
        Self {
            id,
            cwd,
            additional_directories,
            title: None,
            created_at: now.clone(),
            updated_at: now,
            model: model.to_string(),
            think: None,
            tools_enabled: true,
            mode: SessionMode::Default,
            turn_count: 0,
            messages: Vec::new(),
        }
    }

    pub fn fork(&self, new_id: String) -> Self {
        let now = gemini_acp_config::core::time::now_iso();
        Self {
            id: new_id,
            cwd: self.cwd.clone(),
            additional_directories: self.additional_directories.clone(),
            title: self.title.as_ref().map(|t| format!("{t} (fork)")),
            created_at: now.clone(),
            updated_at: now,
            model: self.model.clone(),
            think: self.think,
            tools_enabled: self.tools_enabled,
            mode: self.mode,
            turn_count: 0,
            messages: self.messages.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum TurnError {
    #[error("session introuvable: {0}")]
    NotFound(String),
    #[error("un tour est deja en cours sur cette session — envoyez session/cancel d'abord")]
    AlreadyRunning,
}

pub struct Live {
    pub session: Session,
    pub cancel: tokio::sync::watch::Sender<bool>,
    pub busy: bool,
    pub prompt_handle: Option<tokio::sync::oneshot::Receiver<()>>,
    pub generation: u64,
}
