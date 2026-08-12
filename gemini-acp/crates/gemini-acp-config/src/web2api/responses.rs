//! `POST /v1/responses` (Codex CLI) — port de `GeminiHandler.handle_responses` :
//! `input` (chaîne ou items) → messages OpenAI, `instructions` → system, tools
//! normalisés, sortie `output` (`function_call`/`message`) + usage estimé ;
//! en `stream` : événements SSE `response.created`…`response.completed`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

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

    let messages = convert::responses_input_to_messages(&body);
    let tools = convert::normalize_responses_tools(body.get("tools"));
    let tool_choice = convert::ToolChoice::parse(body.get("tool_choice"));
    // `none` → pas de section tools ni de parsing `tool_call` en sortie.
    let tools_opt = if tools.is_empty() || tool_choice.is_none() {
        None
    } else {
        Some(tools.as_slice())
    };
    let prompt = convert::messages_to_prompt(&messages, tools_opt, &tool_choice);
    if prompt.trim().is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": {"message": "empty input"}}),
        );
    }

    let text = match state
        .client
        .complete(&prompt, &resolved.name, Some(resolved.think), &[])
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

    let (clean, tool_calls) = if tools_opt.is_some() && !text.is_empty() {
        convert::parse_tool_calls(&text)
    } else {
        (text, Vec::new())
    };
    let rid = format!("resp_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    let mid = format!("msg_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut output: Vec<Value> = Vec::new();
    for tc in &tool_calls {
        output.push(serde_json::json!({
            "type": "function_call",
            "id": tc["id"],
            "call_id": tc["id"],
            "name": tc["function"]["name"],
            "arguments": tc["function"]["arguments"],
            "status": "completed",
        }));
    }
    if !clean.is_empty() || tool_calls.is_empty() {
        output.push(serde_json::json!({
            "type": "message",
            "id": mid,
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": clean, "annotations": []}],
        }));
    }

    let usage = serde_json::json!({
        "input_tokens": prompt.chars().count() / 4,
        "output_tokens": clean.chars().count() / 4,
        "total_tokens": (prompt.len() + clean.len()) / 4,
    });
    let base = Arc::new(serde_json::json!({
        "id": rid,
        "object": "response",
        "created_at": created,
        "model": model_name,
    }));

    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        let (tx, out) = sse_channel();
        tokio::spawn(emit_stream(tx, base, output.clone(), usage));
        sse(out).into_response()
    } else {
        let mut result = (*base).clone();
        result["status"] = Value::String("completed".into());
        result["output"] = Value::Array(output);
        result["usage"] = usage;
        json_ok(result)
    }
}

/// Fusionne `base` avec des champs supplémentaires (la macro `json!` ne sait
/// pas étaler un objet).
fn merged(base: &Value, extra: Map<String, Value>) -> Value {
    let mut obj = base.as_object().cloned().unwrap_or_default();
    obj.extend(extra);
    Value::Object(obj)
}

/// Événement SSE : `{type, sequenceNumber, …, response?}` (port de `emit`).
async fn emit(
    tx: &mpsc::Sender<Result<Event, Infallible>>,
    seq: &mut u64,
    base: &Value,
    ev_type: &str,
    fields: Map<String, Value>,
) {
    *seq += 1;
    let payload = merged(
        base,
        Map::from_iter([
            ("type".into(), Value::String(ev_type.into())),
            ("sequence_number".into(), Value::from(*seq)),
        ]),
    );
    let payload = merged(&payload, fields);
    let _ = tx.send(Ok(sse_event(payload))).await;
}

async fn emit_stream(
    tx: mpsc::Sender<Result<Event, Infallible>>,
    base: Arc<Value>,
    output: Vec<Value>,
    usage: Value,
) {
    let mut seq = 0u64;
    let in_progress = || {
        merged(
            &base,
            Map::from_iter([
                ("status".into(), Value::String("in_progress".into())),
                ("output".into(), Value::Array(vec![])),
                ("usage".into(), Value::Null),
            ]),
        )
    };
    emit(
        &tx,
        &mut seq,
        &base,
        "response.created",
        Map::from_iter([("response".into(), in_progress())]),
    )
    .await;
    emit(
        &tx,
        &mut seq,
        &base,
        "response.in_progress",
        Map::from_iter([("response".into(), in_progress())]),
    )
    .await;

    for (oi, item) in output.iter().enumerate() {
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            let pending = serde_json::json!({
                "type": "function_call",
                "id": item["id"],
                "call_id": item["call_id"],
                "name": item["name"],
                "arguments": "",
                "status": "in_progress",
            });
            let mut fields = Map::new();
            fields.insert("output_index".into(), Value::from(oi));
            fields.insert("item".into(), pending);
            emit(&tx, &mut seq, &base, "response.output_item.added", fields).await;
            emit(
                &tx,
                &mut seq,
                &base,
                "response.function_call_arguments.delta",
                Map::from_iter([
                    ("item_id".into(), item["id"].clone()),
                    ("output_index".into(), Value::from(oi)),
                    ("delta".into(), item["arguments"].clone()),
                ]),
            )
            .await;
            emit(
                &tx,
                &mut seq,
                &base,
                "response.function_call_arguments.done",
                Map::from_iter([
                    ("item_id".into(), item["id"].clone()),
                    ("output_index".into(), Value::from(oi)),
                    ("arguments".into(), item["arguments"].clone()),
                ]),
            )
            .await;
            emit(
                &tx,
                &mut seq,
                &base,
                "response.output_item.done",
                Map::from_iter([
                    ("output_index".into(), Value::from(oi)),
                    ("item".into(), item.clone()),
                ]),
            )
            .await;
        } else {
            let pending = serde_json::json!({
                "type": "message",
                "id": item["id"],
                "role": "assistant",
                "status": "in_progress",
                "content": [],
            });
            emit(
                &tx,
                &mut seq,
                &base,
                "response.output_item.added",
                Map::from_iter([
                    ("output_index".into(), Value::from(oi)),
                    ("item".into(), pending),
                ]),
            )
            .await;
            let content = item
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (ci, part) in content.iter().enumerate() {
                let part_text = part.get("text").and_then(Value::as_str).unwrap_or("");
                emit(
                    &tx,
                    &mut seq,
                    &base,
                    "response.content_part.added",
                    Map::from_iter([
                        ("item_id".into(), item["id"].clone()),
                        ("output_index".into(), Value::from(oi)),
                        ("content_index".into(), Value::from(ci)),
                        (
                            "part".into(),
                            serde_json::json!({"type": "output_text", "text": "", "annotations": []}),
                        ),
                    ]),
                )
                .await;
                emit(
                    &tx,
                    &mut seq,
                    &base,
                    "response.output_text.delta",
                    Map::from_iter([
                        ("item_id".into(), item["id"].clone()),
                        ("output_index".into(), Value::from(oi)),
                        ("content_index".into(), Value::from(ci)),
                        ("delta".into(), Value::String(part_text.into())),
                    ]),
                )
                .await;
                emit(
                    &tx,
                    &mut seq,
                    &base,
                    "response.output_text.done",
                    Map::from_iter([
                        ("item_id".into(), item["id"].clone()),
                        ("output_index".into(), Value::from(oi)),
                        ("content_index".into(), Value::from(ci)),
                        ("text".into(), Value::String(part_text.into())),
                    ]),
                )
                .await;
                emit(
                    &tx,
                    &mut seq,
                    &base,
                    "response.content_part.done",
                    Map::from_iter([
                        ("item_id".into(), item["id"].clone()),
                        ("output_index".into(), Value::from(oi)),
                        ("content_index".into(), Value::from(ci)),
                        ("part".into(), part.clone()),
                    ]),
                )
                .await;
            }
            emit(
                &tx,
                &mut seq,
                &base,
                "response.output_item.done",
                Map::from_iter([
                    ("output_index".into(), Value::from(oi)),
                    ("item".into(), item.clone()),
                ]),
            )
            .await;
        }
    }

    emit(
        &tx,
        &mut seq,
        &base,
        "response.completed",
        Map::from_iter([(
            "response".into(),
            merged(
                &base,
                Map::from_iter([
                    ("status".into(), Value::String("completed".into())),
                    ("output".into(), Value::Array(output)),
                    ("usage".into(), usage),
                ]),
            ),
        )]),
    )
    .await;
}
