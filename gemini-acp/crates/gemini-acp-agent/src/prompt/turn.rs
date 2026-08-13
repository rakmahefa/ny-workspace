//! Orchestration d'un tour de conversation.
//!
//! Refactor R1 — inspiré de `glm-acp-agent/src/protocol/agent.ts` :
//!
//! - **ToolExecutor** : utilisation du nouveau `tools::executor::ToolExecutor`
//!   pour le dispatch d'outils avec notifications ACP complètes et permissions.
//! - **Compaction proactive** : avant chaque tour, si l'historique dépasse 90%
//!   de la fenêtre de contexte, on compacte.
//! - **Retry sur overflow** : si l'API retourne une erreur de contexte,
//!   on compacte à 70% et réessaie une fois.
//! - **Stop reason mapping** : mappe les finish reasons Gemini vers les
//!   ACP StopReason.
//! - **Error surfacing** : les erreurs sont envoyées comme `[error]`
//!   `agent_message_chunk`.
//! - **Prompt serialization** : attend la fin du prompt précédent avant de
//!   démarrer.
//! - **safe_session_update** : toutes les notifications ACP passent par
//!   le wrapper défensif.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, MessageId, PromptRequest, PromptResponse, SessionInfoUpdate, SessionUpdate,
    StopReason, TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError, Responder};

use gemini_acp_runtime::state::{Role, Store, TurnError};

use super::build::build_prompt;
use super::content::blocks_to_parts;
use super::error::{actionable_error_message, actionable_stream_error};
use super::follow_up::{request_action, replace_components, StreamNormalizer};
use super::notify::{notify_text, notify_usage};
use super::title::derive_title;
use gemini_acp_runtime::tools::executor::{emit_error_chunk, safe_session_update, ToolExecutor};
use gemini_acp_runtime::tools::parse::parse_tool_calls;
use gemini_acp_runtime::tools::ToolRegistry;

const MAX_TURNS: usize = 20;
const CONTEXT_WINDOW_CHARS: usize = 1_000_000;
const COMPACTION_THRESHOLD_CHARS: usize = (CONTEXT_WINDOW_CHARS as f64 * 0.9) as usize;
const EMERGENCY_COMPACTION_CHARS: usize = (CONTEXT_WINDOW_CHARS as f64 * 0.7) as usize;
const PRESERVE_TURNS: usize = 10;

enum TurnOutcome {
    Complete,
    Cancelled,
    Failed(String),
}

struct TurnGuard {
    store: Arc<Store>,
    session_id: String,
    session: Option<gemini_acp_runtime::state::Session>,
    finished: bool,
    generation: u64,
}

impl TurnGuard {
    fn new(store: Arc<Store>, session_id: String, session: gemini_acp_runtime::state::Session, generation: u64) -> Self {
        Self { store, session_id, session: Some(session), finished: false, generation }
    }

    fn session_mut(&mut self) -> &mut gemini_acp_runtime::state::Session {
        self.session.as_mut().expect("TurnGuard: session déjà consommée")
    }

    async fn finish(mut self) {
        if let Some(session) = self.session.take() {
            let sid = &self.session_id;
            if let Err(e) = self.store.end_turn(sid, session, self.generation).await {
                tracing::warn!(session = %self.session_id, "end_turn a échoué dans TurnGuard: {e}");
            }
        }
        self.finished = true;
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.finished {
            let sid = self.session_id.clone();
            let store = self.store.clone();
            let generation = self.generation;
            if let Some(session) = self.session.take() {
                tokio::spawn(async move {
                    if let Err(e) = store.end_turn(&sid, session, generation).await {
                        tracing::warn!(session = %sid, "TurnGuard::drop: tour obsolète, état non persisté (sûr) : {e}");
                    }
                });
            } else {
                let sid2 = sid.clone();
                let store2 = store.clone();
                tokio::spawn(async move { store2.force_idle(&sid2).await; });
                tracing::warn!(session = %self.session_id, "TurnGuard::drop: session déjà consommée");
            }
        }
    }
}

fn map_stop_reason_from_error(e: &str) -> StopReason {
    let lower = e.to_lowercase();
    if lower.contains("safety") || lower.contains("block") { StopReason::Refusal } else { StopReason::EndTurn }
}

fn compact_messages(messages: &mut Vec<(Role, String)>, target_chars: usize) {
    if messages.len() <= 1 { return; }
    let mut turns: Vec<Vec<(Role, String)>> = Vec::new();
    let mut current_turn = Vec::new();
    for msg in messages.iter() {
        if msg.0 == Role::User && !current_turn.is_empty() { turns.push(std::mem::take(&mut current_turn)); }
        current_turn.push(msg.clone());
    }
    if !current_turn.is_empty() { turns.push(current_turn); }
    if turns.len() <= PRESERVE_TURNS { return; }
    let current_chars: usize = messages.iter().map(|(_, t)| t.len()).sum();
    if current_chars <= target_chars { return; }
    let tail_end = turns.len().saturating_sub(PRESERVE_TURNS);
    let mut candidates: Vec<(usize, usize)> = (0..tail_end)
        .map(|i| (i, turns[i].iter().map(|(_, t)| t.len()).sum::<usize>()))
        .collect();
    candidates.sort_by_key(|b| std::cmp::Reverse(b.1));
    let mut to_evict = std::collections::HashSet::new();
    let mut remaining_chars = current_chars;
    for (idx, turn_chars) in candidates {
        if remaining_chars <= target_chars { break; }
        to_evict.insert(idx);
        remaining_chars -= turn_chars;
    }
    let mut compacted = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        if i < tail_end && to_evict.contains(&i) { continue; }
        compacted.extend(turn.iter().cloned());
    }
    *messages = compacted;
}

/// Déroule un tour `session/prompt` avec boucle outil intégrée.
pub async fn run_turn(
    store: Arc<Store>,
    tools: Arc<ToolRegistry>,
    client: gemini_acp_config::client::Client,
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), AcpError> {
    let session_id = req.session_id.clone();
    let sid = &*session_id.0;
    let span = tracing::info_span!("turn", session = %session_id, chars_input = tracing::field::Empty, chars_output = tracing::field::Empty, tool_rounds = tracing::field::Empty, outcome = tracing::field::Empty);
    let _enter = span.enter();

    let (session, mut cancel, generation) = match store.begin_turn(sid).await {
        Ok(triple) => triple,
        Err(TurnError::NotFound(_)) => return responder.respond_with_error(AcpError::invalid_params().data(serde_json::json!({ "session_id": session_id.to_string() }))),
        Err(TurnError::AlreadyRunning) => return responder.respond_with_error(AcpError::invalid_params().data(serde_json::json!({ "session_id": session_id.to_string(), "error": "a turn is already running; send session/cancel first" }))),
    };

    let mut guard = TurnGuard::new(store.clone(), sid.to_string(), session, generation);
    let session = guard.session_mut();
    let (user_text, images) = blocks_to_parts(&req.prompt);
    span.record("chars_input", user_text.chars().count());
    let message_id = MessageId::from(format!("msg_{}", uuid::Uuid::new_v4().simple()));

    if session.title.is_none() && !user_text.trim().is_empty() {
        let title = derive_title(&user_text);
        session.title = Some(title.clone());
        safe_session_update(&cx, &session_id, SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)));
    }

    let mut refs = Vec::new();
    if !images.is_empty() {
        let total = images.len();
        let upload_call_id = ToolCallId::from(format!("call_{}", uuid::Uuid::new_v4().simple()));
        safe_session_update(&cx, &session_id, SessionUpdate::ToolCall(ToolCall::new(upload_call_id.clone(), format!("Upload {total} image(s) (Scotty)")).kind(ToolKind::Fetch).status(ToolCallStatus::InProgress)));
        for (idx, (b64, mime)) in images.iter().enumerate() {
            match client.upload_image(b64, mime).await {
                Ok(r) => refs.push(r),
                Err(e) => {
                    let content = vec![ToolCallContent::Content(
                        agent_client_protocol::schema::v1::Content::new(ContentBlock::Text(
                            TextContent::new(format!("Upload image {}/{} échoué: {e:#}", idx + 1, total)),
                        )),
                                                        )];
                    safe_session_update(
                        &cx,
                        &session_id,
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            upload_call_id.clone(),
                            ToolCallUpdateFields::new()
                                .status(ToolCallStatus::Failed)
                                .content(content),
                        )),
                    );
                    span.record("outcome", "refusal_upload");
                    return responder.respond(PromptResponse::new(StopReason::Refusal));
                }
            }
        }
        let content = vec![ToolCallContent::Content(
            agent_client_protocol::schema::v1::Content::new(ContentBlock::Text(
                TextContent::new(format!("{total} image(s) uploadée(s) avec succès")),
            )),
        )];
        safe_session_update(
            &cx,
            &session_id,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                upload_call_id,
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(content),
            )),
        );
    }

    session.messages.push((Role::User, user_text.clone()));
    let mut total_output = String::new();
    let cwd = session.cwd.clone();
    let additional_dirs = session.additional_directories.clone();
    let registry = &*tools;
    let mut tool_round = 0usize;
    let mut final_assistant_pushed = false;
    let mut overflow_retry_count = 0usize;
    let session_mode = session.mode;
    let mode_getter = || session_mode;

    for round in 0..MAX_TURNS {
        tool_round = round;
        if *cancel.borrow() {
            span.record("outcome", "cancelled");
            return responder.respond(PromptResponse::new(StopReason::Cancelled));
        }

        let history_chars: usize = session.messages.iter().map(|(_, t)| t.len()).sum();
        if history_chars > COMPACTION_THRESHOLD_CHARS { compact_messages(&mut session.messages, EMERGENCY_COMPACTION_CHARS); }

        let prompt = build_prompt(session, Some(registry));
        let mut rx = match client.stream(&prompt, &session.model, session.think, &refs).await {
            Ok(rx) => rx,
            Err(e) => {
                let note = actionable_error_message(&e);
                let is_overflow = e.to_string().contains("context") || e.to_string().contains("too long") || e.to_string().contains("tokens");
                if is_overflow && overflow_retry_count < 1 {
                    compact_messages(&mut session.messages, EMERGENCY_COMPACTION_CHARS);
                    overflow_retry_count += 1;
                    continue;
                }
                if is_overflow {
                    emit_error_chunk(&cx, &session_id, &message_id, &format!("Context overflow persisted after emergency compaction: {e:#}"));
                    span.record("outcome", "refusal_start");
                    return responder.respond(PromptResponse::new(StopReason::MaxTokens));
                }
                emit_error_chunk(&cx, &session_id, &message_id, &note);
                span.record("outcome", "failed_start");
                return responder.respond(PromptResponse::new(StopReason::EndTurn));
            }
        };

        let is_thinking_model = gemini_acp_config::core::models::resolve(&session.model, gemini_acp_config::core::models::DEFAULT_MODEL)
            .map(|r| gemini_acp_config::core::models::is_thinking_mode(r.mode))
            .unwrap_or(false);
        let mut thought_stream = crate::thought::ThoughtStream::new(is_thinking_model);
        let mut follow_up_stream = StreamNormalizer::default();
        let mut assistant = String::new();
        let outcome = loop {
            tokio::select! {
                _ = cancel.changed() => break TurnOutcome::Cancelled,
                item = rx.recv() => {
                    let Some(item) = item else { break TurnOutcome::Complete };
                    match item {
                        Ok(delta) => {
                            for event in thought_stream.feed(&delta) {
                                match event {
                                    crate::thought::ThoughtEvent::ThoughtChunk(text) => {
                                        crate::thought::notify_thought(&cx, &session_id, &message_id, &text).await?;
                                    }
                                    crate::thought::ThoughtEvent::ThoughtEnd => {}
                                    crate::thought::ThoughtEvent::ResponseChunk(text) => {
                                        assistant.push_str(&text);
                                        let safe_message = follow_up_stream.push(&text);
                                        if !safe_message.is_empty() {
                                            notify_text(&cx, &session_id, &message_id, safe_message)?;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => break TurnOutcome::Failed(e),
                    }
                }
            }
        };
        drop(rx);

        for event in thought_stream.finish() {
            match event {
                crate::thought::ThoughtEvent::ThoughtChunk(text) => {
                    crate::thought::notify_thought(&cx, &session_id, &message_id, &text).await?;
                }
                crate::thought::ThoughtEvent::ThoughtEnd => {}
                crate::thought::ThoughtEvent::ResponseChunk(text) => {
                    assistant.push_str(&text);
                    let safe_message = follow_up_stream.push(&text);
                    if !safe_message.is_empty() {
                        notify_text(&cx, &session_id, &message_id, safe_message)?;
                    }
                }
            }
        }
        let follow_up_tail = follow_up_stream.finish();
        if !follow_up_tail.is_empty() { notify_text(&cx, &session_id, &message_id, follow_up_tail)?; }

        if matches!(outcome, TurnOutcome::Cancelled) {
            span.record("outcome", "cancelled");
            return responder.respond(PromptResponse::new(StopReason::Cancelled));
        }
        if let TurnOutcome::Failed(e) = &outcome {
            emit_error_chunk(&cx, &session_id, &message_id, &actionable_stream_error(e));
            span.record("outcome", "failed");
            return responder.respond(PromptResponse::new(map_stop_reason_from_error(e)));
        }

        let (clean_text, tool_calls) = parse_tool_calls(&assistant);
        let clean_text = replace_components(&clean_text);
        if tool_calls.is_empty() || !session.tools_enabled || !registry.has_tools() {
            total_output = clean_text;
            break;
        }

        tracing::info!(session = %session_id, round = round, tool_count = tool_calls.len(), "tool calls détectés — exécution via ToolExecutor");
        let tool_blocks: String = tool_calls.iter().map(|c| c.to_history_block()).collect::<Vec<_>>().join("\n");
        let assistant_history = if clean_text.is_empty() { tool_blocks } else { format!("{}\n{}", clean_text, tool_blocks) };
        session.messages.push((Role::Assistant, assistant_history));

        let executor = ToolExecutor::new(&cx, &session_id, registry, &cwd, &additional_dirs, &mode_getter);
        let mut follow_up_seen = false;
        let mut follow_up_selected = None;

        for call in &tool_calls {
            if *cancel.borrow() { return responder.respond(PromptResponse::new(StopReason::Cancelled)); }

            if call.name == "FollowUp" {
                follow_up_seen = true;
                let label = call.arguments.get("label").and_then(serde_json::Value::as_str).unwrap_or("Suggested next step").trim();
                let query = call.arguments.get("query").and_then(serde_json::Value::as_str).unwrap_or("").trim();
                if !label.is_empty() && !query.is_empty() {
                    match request_action(&cx, &session_id, label, query).await {
                        Ok(selected) => follow_up_selected = selected,
                        Err(error) => {
                            emit_error_chunk(&cx, &session_id, &message_id, &format!("FollowUp interaction failed: {error}"));
                        }
                    }
                }
                // FollowUp is an interactive action, never an executable tool.
                // Stop processing additional calls in the same model emission.
                break;
            }

            let result = executor.execute(&call.name, &call.arguments).await;
            session.messages.push((Role::Tool, gemini_acp_runtime::tools::prompt::format_tool_result(&call.name, &result.content)));
        }

        if follow_up_seen {
            if let Some(query) = follow_up_selected {
                // The click is a user decision. Feed the selected query back
                // into the same ACP turn as the next user message, rather than
                // fabricating a successful ToolCall result.
                session.messages.push((Role::User, query));
                total_output.clear();
                continue;
            }

            // Dismissed/cancelled FollowUp: preserve the assistant's text and
            // end the current turn normally.
            total_output = clean_text;
            break;
        }

        if round == MAX_TURNS - 1 {
            total_output = "[Limite d'itérations outil atteinte]".into();
            final_assistant_pushed = true;
            break;
        }
    }

    span.record("tool_rounds", tool_round);
    span.record("chars_output", total_output.chars().count());
    if !final_assistant_pushed && !total_output.trim().is_empty() { session.messages.push((Role::Assistant, total_output.clone())); }
    if let Err(e) = notify_usage(&cx, &session_id, &build_prompt(session, Some(registry)), &total_output) {
        tracing::warn!(session = %session_id, "notify_usage a échoué: {e}");
    }
    guard.finish().await;
    span.record("outcome", "end_turn");
    responder.respond(PromptResponse::new(StopReason::EndTurn))
}
