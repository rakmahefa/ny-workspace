//! Types du module state : rôles, modes de session, données persistées, erreurs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Nombre maximum de snapshots conservés par session.
pub const MAX_SNAPSHOTS: usize = 10;

/// Rôle d'un message de l'historique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    /// Résultat d'un appel d'outil (injecté dans l'historique).
    #[serde(rename = "tool")]
    Tool,
}

/// Mode de permission de la session (inspiré de `glm-acp-agent`).
///
/// Contrôle quand l'agent demande la permission utilisateur pour les
/// opérations d'outils qui modifient l'état (écriture fichier, exécution
/// commande).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionMode {
    /// Demande la permission avant toute écriture ou commande.
    #[default]
    #[serde(rename = "default")]
    Default,
    /// Les écritures s'exécutent sans permission, les commandes demandent.
    #[serde(rename = "accept_edits")]
    AcceptEdits,
    /// Toutes les opérations s'exécutent sans permission.
    #[serde(rename = "bypass_permissions")]
    BypassPermissions,
}

impl SessionMode {
    /// Liste des modes valides pour la validation.
    pub fn all() -> &'static [SessionMode] {
        &[
            SessionMode::Default,
            SessionMode::AcceptEdits,
            SessionMode::BypassPermissions,
        ]
    }

    /// Parse depuis une chaîne (insensible à la casse).
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "default" => Some(SessionMode::Default),
            "accept_edits" => Some(SessionMode::AcceptEdits),
            "bypass_permissions" => Some(SessionMode::BypassPermissions),
            _ => None,
        }
    }

    /// Nom lisible pour l'ACP.
    ///
    /// Note : le SDK ACP v2 ne supporte pas `requestPermission` nativement.
    /// En mode Default, les opérations mutatives sont marquées Pending dans
    /// l'UI du client, puis auto-approuvées après timeout (0 ms).
    /// L'infrastructure de canal oneshot est prête pour quand le SDK
    /// ajoutera le support.
    pub fn display_name(&self) -> &'static str {
        match self {
            SessionMode::Default => "Ask for permission (auto-approved)",
            SessionMode::AcceptEdits => "Auto-approve edits",
            SessionMode::BypassPermissions => "Bypass all permissions",
        }
    }

    /// Description pour l'ACP.
    pub fn description(&self) -> &'static str {
        match self {
            SessionMode::Default => "Prompts for edits and commands (auto-approved until SDK supports requestPermission).",
            SessionMode::AcceptEdits => "Edits run without prompting. High-risk commands still prompt.",
            SessionMode::BypassPermissions => "Edits and commands run without prompting.",
        }
    }

    /// Détermine si une écriture nécessite une permission.
    pub fn requires_write_permission(&self) -> bool {
        matches!(self, SessionMode::Default)
    }

    /// Détermine si une exécution nécessite une permission.
    pub fn requires_execute_permission(&self) -> bool {
        !matches!(self, SessionMode::BypassPermissions)
    }
}

/// Session persistée (`sess_<hex>` — cf. conventions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub title: Option<String>,
    /// ISO 8601 UTC.
    pub created_at: String,
    /// ISO 8601 UTC, mise à jour à chaque fin de tour.
    pub updated_at: String,
    pub model: String,
    /// Niveau de réflexion 0..=4 (`None` = défaut du modèle).
    pub think: Option<u32>,
    /// Outils activés pour cette session.
    pub tools_enabled: bool,
    /// Mode de permission (default/accept_edits/bypass_permissions).
    #[serde(default)]
    pub mode: SessionMode,
    /// Nombre de tours complétés (pour le suivi).
    #[serde(default)]
    pub turn_count: u64,
    pub messages: Vec<(Role, String)>,
}

impl Session {
    pub fn new(
        id: String,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        model: &str,
    ) -> Self {
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

    /// Crée un fork de cette session (deep clone des messages, nouvel ID).
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

/// Erreurs typées pour `Store::begin_turn`.
#[derive(Debug, Error)]
pub enum TurnError {
    #[error("session introuvable: {0}")]
    NotFound(String),
    #[error("un tour est deja en cours sur cette session — envoyez session/cancel d'abord")]
    AlreadyRunning,
}

/// Entrée mémoire : session + jeton d'annulation du tour courant + verrou busy.
///
/// NB : pas de derive `Clone` — `JoinHandle` n'est pas clonable. `Store`
/// n'en a pas besoin (la map est derrière `Arc<RwLock<..>>`).
pub struct Live {
    pub session: Session,
    pub cancel: tokio::sync::watch::Sender<bool>,
    /// true si un tour est en cours — empêche `begin_turn` concurrent.
    pub busy: bool,
    /// Handle vers le tour en cours pour la sérialisation des prompts.
    /// `None` si aucun tour n'est actif.
    pub prompt_handle: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Numero de generation : incremente a chaque begin_turn et cancel.
    /// Permet de detecter les tours obsoletes (post-cancel) qui tentent
    /// de persister pendant qu'un nouveau tour est deja en cours.
    pub generation: u64,
}
