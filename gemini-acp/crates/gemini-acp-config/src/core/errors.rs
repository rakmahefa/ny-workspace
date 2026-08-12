//! Erreurs typées partagées (refactor M6 — cf. spec §5.4).
//!
//! Les erreurs Gemini sont typées via `thiserror` pour permettre aux clients
//! (notamment `acp::prompt::run_turn`) de `match` sur des cas spécifiques et
//! renvoyer un message ACP actionnable à l'utilisateur (cookies expirés,
//! modèle inconnu, divergence de flux, etc.) au lieu d'un `anyhow` générique.

use thiserror::Error;

/// Erreur du backend Gemini web. Couvre les cas identifiés dans
/// `gemini_client::Client::stream` / `attempt_http` / `emit_delta`.
#[derive(Debug, Error)]
pub enum GeminiError {
    /// Cookies expirés ou invalides — `BardErrorInfo [<code>]` dans le corps.
    /// Code 401 = cookies expirés, autres codes = erreur amont Google.
    #[error(
        "cookies expires ou invalides (BardErrorInfo [{code}]) — reexportez vendor/cookie.json"
    )]
    CookiesExpired { code: i64 },

    /// Modèle inconnu — la clé n'est pas dans la table `gemini_core::models`.
    #[error("modele inconnu: {0}")]
    UnknownModel(String),

    /// Erreur réseau (timeout, DNS, connexion rompue).
    #[error("erreur reseau: {0}")]
    Network(String),

    /// Erreur HTTP (status non-2xx).
    #[error("erreur HTTP {status}: {body}")]
    Http { status: u16, body: String },

    /// Divergence de flux en cours de streaming — le texte cumulé a change
    /// de prefix après émission, retry impossible.
    #[error("divergence de flux en cours de streaming")]
    StreamDivergence,

    /// Upload Scotty échoué (initiation ou finalisation).
    #[error("upload Scotty echec: {0}")]
    UploadFailed(String),

    /// Blocage par la politique de sécurité de Gemini (blockReason, refus
    /// textuel, ou flux vide sans candidat). Le champ contient la raison
    /// lisible pour l'utilisateur.
    #[error("{0}")]
    SafetyBlocked(String),

    /// Erreur non classée — wrap `anyhow::Error` pour compatibilité ascendante.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type GeminiResult<T> = Result<T, GeminiError>;
