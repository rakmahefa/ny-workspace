//! Middleware CORS + auth par clé API (port de `_authorized` du vendor),
//! et helpers de réponse JSON / flux SSE (`data: …\n\n`, flush par chunk).

use std::convert::Infallible;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAliveStream, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::config::Config;

/// Item d'un canal SSE (événement déjà construit, jamais d'erreur).
pub type SseItem = Result<Event, Infallible>;
/// Canal (émetteur → récepteur) d'un flux SSE.
pub type SseChannel = (mpsc::Sender<SseItem>, mpsc::Receiver<SseItem>);

/// Taille maximale du corps de requête JSON (16 Mio). Au-delà, on renvoie 400.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// En-têtes autorisés pour les requêtes CORS. Restreindre à la liste
/// effectivement utilisée par les clients OpenAI/Codex/Gemini CLI plutôt que
/// d'accepter `*` réduit la surface d'attaque (S4).
const CORS_ALLOW_HEADERS: &str = "Authorization, Content-Type, x-api-key, x-goog-api-key, Accept";

/// Comparaison temporellement constante de deux chaînes (S3). Empêche les
/// attaques par timing qui pourraient divulguer la clé API octet par octet.
/// On utilise une implémentation simple basée sur XOR cumulé (plutôt que
/// d'ajouter une dépendance `subtle`) : tant que les longueurs diffèrent on
/// continue à XORer pour garder un temps d'exécution indépendant du contenu.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        // On dérive quand même un accumuleur pour garder un temps constant
        // indépendamment du moment où on détecte la différence de longueur.
        let mut acc: u8 = 0xff;
        for byte in ab {
            acc ^= byte;
        }
        for byte in bb {
            acc ^= byte;
        }
        let _ = acc; // empêche le compilateur d'élaguer la boucle
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in ab.iter().zip(bb.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

#[derive(Clone)]
pub struct AppState {
    pub client: gemini_acp_config::client::Client,
    pub config: std::sync::Arc<Config>,
}

/// Réponse JSON avec CORS `*`, comme `send_json` du vendor.
pub fn json_response(status: StatusCode, data: Value) -> Response {
    let body = serde_json::to_string(&data).unwrap_or_else(|_| "{}".into());
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

pub fn json_ok(data: Value) -> Response {
    json_response(StatusCode::OK, data)
}

/// Canal d'un flux SSE ; chaque `send` produit `data: <json>\n\n` (flush par
/// chunk assuré par axum). Couplé à [`sse`] ci-dessous.
pub fn sse_channel() -> SseChannel {
    mpsc::channel(16)
}

pub fn sse_event(data: Value) -> Event {
    Event::default().data(serde_json::to_string(&data).unwrap_or_else(|_| "{}".into()))
}

/// Corps SSE (`text/event-stream`, keep-alive 15 s).
pub fn sse(rx: mpsc::Receiver<SseItem>) -> Sse<KeepAliveStream<ReceiverStream<SseItem>>> {
    Sse::new(ReceiverStream::new(rx)).keep_alive(
        axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
    )
}

/// Middleware : OPTIONS → 204 CORS ; auth si `api_keys` non vide et route
/// `/v1*` ; sinon passe et ajoute `Access-Control-Allow-Origin: *`.
///
/// Note (S4) : l'origine `*` est intentionnellement permissive car le serveur
/// est destiné à un usage local. En cas d'exposition réseau, configurer un
/// proxy inverse avec contrôle d'origine, ou restreindre `Config::bind`
/// à `127.0.0.1` (désormais le défaut, voir `config.rs`).
pub async fn cors_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let config = &state.config;
    if req.method() == Method::OPTIONS {
        return (
            StatusCode::NO_CONTENT,
            [
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                (header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS"),
                (header::ACCESS_CONTROL_ALLOW_HEADERS, CORS_ALLOW_HEADERS),
            ],
        )
            .into_response();
    }

    let path = req.uri().path().to_string();
    if !config.api_keys.is_empty() && path.starts_with("/v1") && !authorized(&req, config) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"error": {"message": "invalid api key"}}),
        );
    }
    let mut response = next.run(req).await;
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
}

/// Port de `_authorized` : Bearer, `x-api-key`/`x-goog-api-key`, `?key=`.
/// Toutes les comparaisons de clé utilisent `constant_time_eq` (S3).
///
/// Note (S5) : l'authentification par `?key=` est conservée pour compat
/// Gemini CLI, mais elle expose la clé dans les logs/referrer. À éviter
/// autant que possible ; préférer l'en-tête `x-goog-api-key`.
fn authorized(req: &Request, config: &Config) -> bool {
    if let Some(auth) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(auth) = auth.to_str() {
            if let Some(key) = auth.strip_prefix("Bearer ") {
                if config.api_keys.iter().any(|k| constant_time_eq(k, key)) {
                    return true;
                }
            }
        }
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if let Some(v) = req.headers().get(name) {
            if let Ok(v) = v.to_str() {
                if config.api_keys.iter().any(|k| constant_time_eq(k, v)) {
                    return true;
                }
            }
        }
    }
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(key) = pair.strip_prefix("key=") {
                if config.api_keys.iter().any(|k| constant_time_eq(k, key)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Corps JSON d'une requête (400 si invalide).
pub async fn json_body(req: Request) -> Result<Value, Response> {
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return Err(json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": {"message": "corps illisible"}}),
            ))
        }
    };
    serde_json::from_slice(&bytes).map_err(|_| {
        json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": {"message": "corps JSON invalide"}}),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_egalite() {
        assert!(constant_time_eq("sk-abc", "sk-abc"));
        assert!(constant_time_eq("", ""));
        assert!(constant_time_eq("a", "a"));
    }

    #[test]
    fn constant_time_eq_difference() {
        assert!(!constant_time_eq("sk-abc", "sk-abd"));
        assert!(!constant_time_eq("sk-abc", "sk-abcX"));
        assert!(!constant_time_eq("sk-abc", ""));
        assert!(!constant_time_eq("", "sk-abc"));
        assert!(!constant_time_eq("sk-abc", "sk-ABC"));
    }
}
