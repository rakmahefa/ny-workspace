//! Tour de conversation : assemblage du prompt multi-tour (spec §3.4 + refactor
//! M8) et orchestration d'une requête Gemini vers les notifications ACP (§3.3
//! + refactor M7).
//!
//! Architecture modulaire :
//! - [`build`]    — construction du prompt (système + historique + fenêtre glissante).
//! - [`content`]  — conversion `ContentBlock` ACP → texte + images.
//! - [`title`]    — dérivation automatique du titre de session.
//! - [`error`]    — messages d'erreur actionnables (cookies, modèle, etc.).
//! - [`notify`]   — notifications ACP (chunks texte, usage tokens).
//! - [`turn`]     — orchestrateur du tour complet (stream, upload, finalisation).

pub mod build;
pub mod content;
pub mod error;
pub mod notify;
pub mod title;
pub mod turn;

// Re-exports publics pour compatibilité avec `agent.rs` (`prompt::run_turn`)
// et tout usage externe éventuel.
pub use turn::run_turn;
