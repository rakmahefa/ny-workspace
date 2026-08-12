//! Endpoints Google natifs (Gemini CLI) — port de `_handle_google_generate` /
//! `_handle_google_models_list` :
//!
//! - `GET  /v1beta/models` → liste au format Google AI.
//! - `POST /v1beta/models/{model}:generateContent` → réponse complète.
//! - `POST /v1beta/models/{model}:streamGenerateContent` → deltas SSE puis
//!   chunk final avec `finishReason`/`usageMetadata` (spec §5.2).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use super::convert;
use super::http::{json_body, json_ok, json_response, sse, sse_channel, sse_event, AppState};

pub async fn models_list() -> Response {
    let models: Vec<Value> = gemini_acp_config::core::models::MODEL_KEYS
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": format!("models/{name}"),
                "displayName": name,
                "description": name,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        })
        .collect();
    json_ok(serde_json::json!({ "models": models }))
}

pub async fn generate(
    State(state): State<AppState>,
    Path(model_path): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let body = match json_body(req).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    // Le suffixe `:generateContent` / `:streamGenerateContent` arrive dans le
    // segment `{model}` (axum ne matche pas `:` comme séparateur).
    let (model_name, stream) = if let Some(n) = model_path.strip_suffix(":streamGenerateContent") {
        (n.to_string(), true)
    } else if let Some(n) = model_path.strip_suffix(":streamGenerate") {
        // Variante courte tolérée (spec §5.2).
        (n.to_string(), true)
    } else if let Some(n) = model_path.strip_suffix(":generateContent") {
        (n.to_string(), false)
    } else {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": {"message": "model not specified in path"}}),
        );
    };

    let resolved = match convert::resolve_model_strict(&model_name, &state.config.default_model) {
        Ok(r) => r,
        Err(msg) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": {"message": msg}}),
            )
        }
    };

    // Tools actifs ? (`toolConfig.functionCallingConfig.mode == NONE` désactive,
    // comme le vendor) — conditionne la section tools et le parsing de sortie.
    let fc_mode = body
        .get("toolConfig")
        .and_then(|c| c.get("functionCallingConfig"))
        .and_then(|c| c.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("AUTO");
    let has_tools = body.get("tools").is_some() && fc_mode != "NONE";

    let (prompt, images) = convert::google_contents_to_prompt(&body);
    if prompt.trim().is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": {"message": "empty content"}}),
        );
    }

    // Upload Scotty des images `inlineData` → refs (`inner[0][3]`). Échecs
    // ignorés avec un warning, comme `_upload_images` du vendor.
    let mut refs = Vec::new();
    for (b64, mime) in images {
        match state.client.upload_image(&b64, &mime).await {
            Ok(r) => refs.push(r),
            Err(e) => tracing::warn!("upload d'image ignoré: {e:#}"),
        }
    }

    if stream && !has_tools {
        return stream_chunks(&state, &prompt, &refs, &resolved, &model_name).await;
    }

    let text = match state
        .client
        .complete(&prompt, &resolved.name, Some(resolved.think), &refs)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            return json_response(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": {"message": format!("upstream error: {e}")}}),
            )
        }
    };
    json_ok(response_object(&text, &model_name, prompt.len(), has_tools))
}

/// Streaming par deltas : chaque delta → `candidates[0].content.parts[0].text`,
/// puis chunk final `finishReason: "STOP"` + `usageMetadata` (spec §5.2).
async fn stream_chunks(
    state: &AppState,
    prompt: &str,
    refs: &[String],
    resolved: &gemini_acp_config::core::models::Resolved,
    model_name: &str,
) -> Response {
    let mut rx = match state
        .client
        .stream(prompt, &resolved.name, Some(resolved.think), refs)
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            return json_response(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": {"message": format!("upstream error: {e}")}}),
            )
        }
    };
    let (tx, out) = sse_channel();
    let model_name = model_name.to_string();
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        let mut emitted = String::new();
        while let Some(item) = rx.recv().await {
            let Ok(delta) = item else {
                tracing::warn!("stream generateContent interrompu: {:?}", item.err());
                break;
            };
            emitted.push_str(&delta);
            let chunk = serde_json::json!({
                "candidates": [{
                    "content": {"parts": [{"text": delta}], "role": "model"},
                    "index": 0,
                }]
            });
            if tx.send(Ok(sse_event(chunk))).await.is_err() {
                return; // client parti → drop du receiver amont (abort HTTP)
            }
        }
        let final_chunk = serde_json::json!({
            "candidates": [{
                "content": {"parts": [{"text": ""}], "role": "model"},
                "finishReason": "STOP",
                "index": 0,
            }],
            "usageMetadata": {
                "promptTokenCount": prompt.chars().count() / 4,
                "candidatesTokenCount": emitted.chars().count() / 4,
                "totalTokenCount": (prompt.len() + emitted.len()) / 4,
            },
            "modelVersion": model_name,
        });
        let _ = tx.send(Ok(sse_event(final_chunk))).await;
    });
    sse(out).into_response()
}

/// Objet réponse complet `generateContent` (port du vendor). Avec tools, la
/// sortie est découpée en parts `text` + `functionCall` (`parse_google_function_calls`).
fn response_object(text: &str, model_name: &str, prompt_len: usize, has_tools: bool) -> Value {
    let parts: Vec<Value> = if has_tools {
        let (clean, calls) = convert::parse_google_function_calls(text);
        let mut parts = Vec::new();
        if !clean.is_empty() {
            parts.push(json!({ "text": clean }));
        }
        for fc in calls {
            parts.push(json!({
                "functionCall": {
                    "name": fc.get("name"),
                    "args": fc.get("args"),
                }
            }));
        }
        if parts.is_empty() {
            parts.push(json!({ "text": text }));
        }
        parts
    } else {
        vec![json!({ "text": text })]
    };
    let candidate = serde_json::json!({
        "content": {"parts": parts, "role": "model"},
        "finishReason": "STOP",
        "index": 0,
    });
    let usage = serde_json::json!({
        "promptTokenCount": prompt_len / 4,
        "candidatesTokenCount": text.len() / 4,
        "totalTokenCount": (prompt_len + text.len()) / 4,
    });
    serde_json::json!({
        "candidates": [candidate],
        "usageMetadata": usage,
        "modelVersion": model_name,
    })
}
