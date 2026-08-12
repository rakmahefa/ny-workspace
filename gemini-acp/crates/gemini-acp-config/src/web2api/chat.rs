//! `POST /v1/chat/completions` (OpenAI) — port de `GeminiHandler.handle_chat`
//! (vérité = vendor) : conversion des messages, streaming SSE réel sans tools,
//! chunk unique + `[DONE]` avec tools (le parse complet est nécessaire).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use super::convert;
use super::http::{json_body, json_ok, json_response, sse, sse_channel, sse_event, AppState};

pub async fn handler(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let body = match json_body(req).await {
        Ok(b) => b,
        Err(e) => return e,
    };

    let model_name = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&state.config.default_model);
    let resolved = match convert::resolve_model_strict(model_name, &state.config.default_model) {
        Ok(r) => r,
        Err(msg) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": {"message": msg}}),
            )
        }
    };

    let messages: Vec<Value> = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tools: Option<Vec<Value>> = body.get("tools").and_then(Value::as_array).cloned();
    let tool_choice = convert::ToolChoice::parse(body.get("tool_choice"));
    // `none` → pas de section tools ni de parsing `tool_call` en sortie.
    let has_tools = tools.is_some() && !tool_choice.is_none();
    let prompt = convert::messages_to_prompt(&messages, tools.as_deref(), &tool_choice);
    if prompt.trim().is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": {"message": "empty prompt"}}),
        );
    }

    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let ctx = Ctx {
        model_name: model_name.to_string(),
        cid: format!(
            "chatcmpl-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };

    if stream && !has_tools {
        return stream_deltas(&state, &prompt, &resolved, &ctx).await;
    }
    let tools_opt = if has_tools { tools.as_deref() } else { None };
    complete(&state, &prompt, &resolved, &ctx, stream, tools_opt).await
}

/// Identifiants de la réponse en cours (`model`, `id` de complétion, horodatage).
struct Ctx {
    model_name: String,
    cid: String,
    created: u64,
}

/// Streaming SSE réel : rôle, deltas du flux, chunk final `finish_reason: stop`,
/// `data: [DONE]`. Une erreur amont avant tout delta termine proprement le flux.
async fn stream_deltas(
    state: &AppState,
    prompt: &str,
    resolved: &gemini_acp_config::core::models::Resolved,
    ctx: &Ctx,
) -> Response {
    let mut rx = match state
        .client
        .stream(prompt, &resolved.name, Some(resolved.think), &[])
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
    let model_name = ctx.model_name.clone();
    let cid = ctx.cid.clone();
    let created = ctx.created;
    tokio::spawn(async move {
        let chunk = |delta: Option<&str>, finish: Option<&str>| {
            let d = match delta {
                Some(t) => serde_json::json!({"content": t}),
                None => serde_json::json!({}),
            };
            sse_event(serde_json::json!({
                "id": cid,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{"index": 0, "delta": d, "finish_reason": finish}],
            }))
        };
        let _ = tx.send(Ok(chunk(None, None))).await;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(delta) => {
                    if tx.send(Ok(chunk(Some(&delta), None))).await.is_err() {
                        return; // client parti → drop du receiver amont (abort HTTP)
                    }
                }
                Err(e) => {
                    // Erreur amont en cours de flux : on s'arrête (comme le vendor).
                    tracing::warn!("stream chat interrompu: {e}");
                    break;
                }
            }
        }
        let _ = tx.send(Ok(chunk(None, Some("stop")))).await;
        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
    });
    sse(out).into_response()
}

/// Réponse complète : `complete` du client, puis `parse_tool_calls` si tools.
/// En mode stream (avec tools) : un seul chunk + `[DONE]`, comme le vendor.
async fn complete(
    state: &AppState,
    prompt: &str,
    resolved: &gemini_acp_config::core::models::Resolved,
    ctx: &Ctx,
    stream: bool,
    tools: Option<&[Value]>,
) -> Response {
    let text = match state
        .client
        .complete(prompt, &resolved.name, Some(resolved.think), &[])
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

    let (clean, tool_calls) = if tools.is_some() && !text.is_empty() {
        convert::parse_tool_calls(&text)
    } else {
        (text, Vec::new())
    };
    let mut msg = serde_json::json!({"role": "assistant", "content": serde_json::Value::Null});
    if !clean.is_empty() {
        msg["content"] = Value::String(clean.clone());
    }
    if let Some(calls) = (!tool_calls.is_empty()).then_some(&tool_calls) {
        msg["tool_calls"] = Value::Array(calls.clone());
    }
    let finish = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };

    if stream {
        let (tx, out) = sse_channel();
        let payload = serde_json::json!({
            "id": ctx.cid,
            "object": "chat.completion.chunk",
            "created": ctx.created,
            "model": ctx.model_name,
            "choices": [{"index": 0, "delta": msg, "finish_reason": finish}],
        });
        tokio::spawn(async move {
            let _ = tx.send(Ok(sse_event(payload))).await;
            let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
        });
        sse(out).into_response()
    } else {
        json_ok(serde_json::json!({
            "id": ctx.cid,
            "object": "chat.completion",
            "created": ctx.created,
            "model": ctx.model_name,
            "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
            "usage": convert::usage(prompt, &clean),
        }))
    }
}
