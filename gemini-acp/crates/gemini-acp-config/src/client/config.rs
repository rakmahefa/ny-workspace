//! Configuration et types internes du client Gemini.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::core::cookies::CookieJar;
use tokio::sync::RwLock;

/// Valeur `bl` courante (change à chaque déploiement Google — centralisée ici).
pub const DEFAULT_BL: &str = "boq_assistant-bard-web-server_20260716.08_p0";

pub(crate) const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36";
pub(crate) const TOKEN_TTL: Duration = Duration::from_secs(600);
pub(crate) const ENDPOINT: &str = "_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate";

/// Endpoint d'initiation de l'upload Scotty (images, cf. §4.2 — refs).
pub(crate) const UPLOAD_ENDPOINT: &str = "https://content-push.googleapis.com/upload/";

/// Repli si la page `/app` ne fournit pas les jetons de l'upload Scotty.
pub(crate) const DEFAULT_PUSH_ID: &str = "feeds/mcudyrk2a4khkz";
pub(crate) const DEFAULT_PCTX: &str = "CgcSBWjK7pYx";

/// Garde-fou : taille maximale d'une image base64 (≈ 24 Mo décodés).
pub(crate) const MAX_IMAGE_B64: usize = 32 * 1024 * 1024;

/// Hôte attendu pour l'URL d'upload Scotty renvoyée par l'initiation.
pub(crate) const UPLOAD_HOST: &str = "content-push.googleapis.com";

/// Compteur global monotone pour `_reqid`.
use std::sync::atomic::AtomicU64;
pub(crate) static REQID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Config {
    /// Fichier de cookies : export EditThisCookie, objet `{cookie, sapisid}`,
    /// ou chaîne brute `k=v; k2=v2`.
    pub cookie_file: PathBuf,
    pub default_model: String,
    /// Compte Google (`/u/<n>`) — absent = compte par défaut.
    pub auth_user: Option<u32>,
    pub proxy: Option<String>,
    pub bl: String,
    pub request_timeout: Duration,
    pub retry_attempts: u32,
    pub retry_delay: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cookie_file: PathBuf::from("vendor/cookie.json"),
            default_model: crate::core::models::DEFAULT_MODEL.to_string(),
            auth_user: None,
            proxy: None,
            bl: DEFAULT_BL.to_string(),
            request_timeout: Duration::from_secs(180),
            retry_attempts: 3,
            retry_delay: Duration::from_secs(2),
        }
    }
}

/// Item du flux : delta de texte, ou erreur (amont) en dernier item.
pub type StreamItem = Result<String, String>;

/// Sous-ensemble des jetons de la page `/app` (cache ~10 min, best effort).
#[derive(Debug, Clone, Default)]
pub(crate) struct PageTokens {
    /// `SNlM0e` — token `at` des requêtes `StreamGenerate`.
    pub(crate) at: Option<String>,
    /// `qKIAYe` — `Push-ID` de l'upload Scotty.
    pub(crate) push_id: Option<String>,
    /// `Ylro7b` — `X-Client-Pctx` de l'upload Scotty.
    pub(crate) pctx: Option<String>,
}

/// Données internes partagées du client.
pub struct ClientInner {
    pub(crate) http: reqwest::Client,
    pub(crate) config: Config,
    /// CookieJar chargé + mtime du fichier (rechargé si le fichier change).
    pub(crate) jar: RwLock<(Option<CookieJar>, Option<SystemTime>)>,
    /// Jetons de page `/app` avec leur horodatage.
    pub(crate) page: RwLock<Option<(PageTokens, Instant)>>,
}
