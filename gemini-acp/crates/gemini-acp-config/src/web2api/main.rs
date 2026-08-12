//! Face API — port Rust de `gemini-web2api` (spec §5) : serveur axum servant
//! le même backend Gemini web. Endpoints :
//!
//! - `GET  /` → statut + modèles.
//! - `GET  /v1/models` → liste au format OpenAI.
//! - `POST /v1/chat/completions` (OpenAI, streaming SSE).
//! - `POST /v1/responses` (Codex CLI, streaming SSE).
//! - `GET  /v1beta/models` + `POST /v1beta/models/{model}:generateContent`
//!   (`:streamGenerateContent`) — Gemini CLI.
//!
//! Configuration : `config.json` (./config.json ou ~/.config/gemini-web2api/)
//! puis env `GEMINI_WEB2API_*` (spec §5.1). Les logs vont sur stderr.

mod chat;
mod config;
mod convert;
mod google;
mod http;
mod responses;

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tracing_subscriber::EnvFilter;

use crate::http::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = std::sync::Arc::new(config::load());
    convert::warn_xsrf_ignored(config.xsrf_token.as_deref());

    let client = gemini_acp_config::client::Client::new(gemini_acp_config::client::Config {
        cookie_file: config
            .cookie_file
            .clone()
            .unwrap_or_else(|| "vendor/cookie.json".into()),
        default_model: config.default_model.clone(),
        auth_user: config.auth_user,
        proxy: config.proxy.clone(),
        bl: config.gemini_bl.clone(),
        request_timeout: Duration::from_secs(config.request_timeout_sec),
        retry_attempts: config.retry_attempts,
        retry_delay: Duration::from_secs(config.retry_delay_sec),
    })
    .await?;

    let state = AppState { client, config };
    let app = Router::new()
        .route("/", get(root))
        .route("/v1/models", get(openai_models))
        .route("/v1beta/models", get(google::models_list))
        .route("/v1/chat/completions", post(chat::handler))
        .route("/v1/responses", post(responses::handler))
        .route("/v1beta/models/{model}", post(google::generate))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            http::cors_auth,
        ))
        .with_state(state.clone());

    let addr = format!("{}:{}", state.config.host, state.config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        "gemini-web2api {} sur http://{addr} (modèle par défaut: {})",
        env!("CARGO_PKG_VERSION"),
        state.config.default_model
    );
    axum::serve(listener, app).await?;
    Ok(())
}

/// `GET /` → `{"status": "ok", "version": …, "models": […]}`.
async fn root(State(state): State<AppState>) -> axum::response::Response {
    let models: Vec<String> = gemini_acp_config::core::models::MODEL_KEYS
        .iter()
        .map(|s| s.to_string())
        .collect();
    http::json_ok(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "models": models,
        "defaultModel": state.config.default_model,
    }))
}

/// `GET /v1/models` (port du vendor : `object: "list"` + `data` descripteurs).
async fn openai_models() -> axum::response::Response {
    let data: Vec<serde_json::Value> = gemini_acp_config::core::models::MODEL_KEYS
        .iter()
        .map(|name| {
            serde_json::json!({
                "id": name,
                "object": "model",
                "created": 1_700_000_000,
                "owned_by": "google",
                "description": name,
            })
        })
        .collect();
    http::json_ok(serde_json::json!({ "object": "list", "data": data }))
}

/// 404 au format du vendor (`{"error": "not found"}`).
async fn not_found() -> axum::response::Response {
    http::json_response(
        StatusCode::NOT_FOUND,
        serde_json::json!({"error": "not found"}),
    )
}
