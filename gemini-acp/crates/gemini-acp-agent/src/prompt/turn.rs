//! Orchestration d'un tour de conversation.
//!
//! Refactor R1 — inspiré de `glm-acp-agent/src/protocol/agent.ts` :
//!
//! - **ToolExecutor** : utilisation du nouveau `tools::executor::ToolExecutor`
//!   pour le dispatch d'outils avec notifications ACP complètes et permissions.
//! - **Compaction proactive** : avant chaque tour, si l'historique dépasse 90%
//!   de la fenêtre de contexte, on compacte (inspiré de
//!   `GlmAcpAgent.runPromptLoop` qui appelle `compactMessages`).
//! - **Retry sur overflow** : si l'API retourne une erreur de contexte,
//!   on compacte à 70% et réessaie une fois (inspiré de `ERR_CONTEXT_OVERFLOW`).
//! - **Stop reason mapping** : mappe les finish reasons Gemini vers les
//!   ACP StopReason via `tools::executor::map_stop_reason`.
//! - **Error surfacing** : les erreurs sont envoyées comme `[error]`
//!   `agent_message_chunk` (inspiré de `GlmAcpAgent.prompt` catch block).
//! - **Prompt serialization** : attend la fin du prompt précédent avant de
//!   démarrer (inspiré de `GlmAcpAgent.promptPromise`).
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
use super::notify::{notify_text, notify_usage};
use super::title::derive_title;
use gemini_acp_runtime::tools::executor::{emit_error_chunk, safe_session_update, ToolExecutor};
use gemini_acp_runtime::tools::parse::parse_tool_calls;
use gemini_acp_runtime::tools::ToolRegistry;

/// Nombre maximal d'itérations outil → résultat → re-prompt.
/// glm-acp-agent utilise 20, mais Gemini a des coûts plus élevés par token.
/// (Const historique — la boucle utilise MAX_TURNS, mais cette valeur
/// est conservée comme référence pour d'éventuels ajustements futurs.)
#[allow(dead_code)]
const MAX_TOOL_ROUNDS: usize = 10;

/// Nombre maximal de tours pour la compaction (glm-acp-agent: 20).
const MAX_TURNS: usize = 20;

/// Mappe une erreur de stream vers un StopReason ACP.
/// Centralise la logique de détection (Safety, block, etc.) qui était
/// dupliquée et fragile dans le handler d'erreur.
fn map_stop_reason_from_error(e: &str) -> StopReason {
    let lower = e.to_lowercase();
    if lower.contains("safety") || lower.contains("block") {
        StopReason::Refusal
    } else {
        StopReason::EndTurn
    }
}

/// Taille estimée de la fenêtre de contexte Gemini (en caractères).
/// Gemini 2.5 Pro ≈ 1M tokens ≈ 4M chars. On utilise 1M chars comme limite
/// conservatrice. La compaction se déclenche à 90% de cette valeur.
/// (Inspiré de `GlmAcpAgent.getContextWindow` + `estimateTokens`)
const CONTEXT_WINDOW_CHARS: usize = 1_000_000;

/// Seuil de compaction proactive (90% de la fenêtre).
const COMPACTION_THRESHOLD_CHARS: usize = (CONTEXT_WINDOW_CHARS as f64 * 0.9) as usize;

/// Seuil de compaction d'urgence (70% de la fenêtre, après overflow).
const EMERGENCY_COMPACTION_CHARS: usize = (CONTEXT_WINDOW_CHARS as f64 * 0.7) as usize;

/// Nombre de tours à préserv lors de la compaction (glm-acp-agent: 10).
const PRESERVE_TURNS: usize = 10;

/// Résultat d'un tour de conversation.
enum TurnOutcome {
    Complete,
    Cancelled,
    Failed(String),
}

/// Guard qui garantit l'appel à `store.end_turn` et `release_busy` même en
/// cas de panique, d'erreur ou de retour anticipé dans `run_turn`.
struct TurnGuard {
    store: Arc<Store>,
    session_id: String,
    session: Option<gemini_acp_runtime::state::Session>,
    finished: bool,
    generation: u64,
}

impl TurnGuard {
    fn new(
        store: Arc<Store>,
        session_id: String,
        session: gemini_acp_runtime::state::Session,
        generation: u64,
    ) -> Self {
        Self {
            store,
            session_id,
            session: Some(session),
            finished: false,
            generation,
        }
    }

    fn session_mut(&mut self) -> &mut gemini_acp_runtime::state::Session {
        self.session
            .as_mut()
            .expect("TurnGuard: session déjà consommée")
    }

    /// Termine le tour normalement avec persistance.
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
            let generation = self.generation; // génération RÉELLE du tour
            if let Some(session) = self.session.take() {
                tokio::spawn(async move {
                    if let Err(e) = store.end_turn(&sid, session, generation).await {
                        // Tour obsolète (annulé puis nouveau tour/cancel) :
                        // l'entrée live appartient à une autre génération.
                        // NE PAS force_idle : busy/sentinel sont gérés par le
                        // tour courant (cancel l'a déjà libéré).
                        tracing::warn!(session = %sid, "TurnGuard::drop: tour obsolète, état non persisté (sûr) : {e}");
                    }
                });
            } else {
                // Defensif : session déjà consommée sans finish() — inatteignable
                // en pratique (take() n'a lieu que dans finish()).
                let sid2 = sid.clone();
                let store2 = store.clone();
                tokio::spawn(async move {
                    store2.force_idle(&sid2).await;
                });
                tracing::warn!(session = %self.session_id, "TurnGuard::drop: session déjà consommée");
            }
        }
    }
}

/// Estime le nombre de tokens à partir des caractères.
/// Règle simple : 4 caractères ≈ 1 token (inspiré de glm-acp-agent `estimateTokens`).
/// Utilisée comme référence pour les heuristiques de compaction.
#[allow(dead_code)]
fn estimate_tokens(messages: &[(Role, String)]) -> usize {
    let chars: usize = messages.iter().map(|(_, text)| text.len()).sum();
    chars / 4
}

/// Compaction proactive de l'historique des messages.
///
/// Inspiré de `GlmAcpAgent.compactMessages()` :
/// 1. Garde toujours le premier message (système).
/// 2. Garde les derniers `PRESERVE_TURNS` groupes d'interaction.
/// 3. Évince les plus gros groupes restants jusqu'à être sous `target_chars`.
fn compact_messages(messages: &mut Vec<(Role, String)>, target_chars: usize) {
    if messages.len() <= 1 {
        return;
    }

    // Grouper les messages en tours (un tour commence par un message User).
    let mut turns: Vec<Vec<(Role, String)>> = Vec::new();
    let mut current_turn: Vec<(Role, String)> = Vec::new();

    for msg in messages.iter() {
        if msg.0 == Role::User && !current_turn.is_empty() {
            turns.push(std::mem::take(&mut current_turn));
        }
        current_turn.push(msg.clone());
    }
    if !current_turn.is_empty() {
        turns.push(current_turn);
    }

    if turns.len() <= PRESERVE_TURNS {
        return;
    }

    // Vérifier si on dépasse la cible.
    let current_chars: usize = messages.iter().map(|(_, t)| t.len()).sum();
    if current_chars <= target_chars {
        return;
    }

    tracing::debug!(
        current_chars = current_chars,
        target = target_chars,
        turns = turns.len(),
        "compact_messages: compaction déclenchée"
    );

    // Identifier les candidats à l'éviction (tout sauf les derniers PRESERVE_TURNS).
    let tail_end = turns.len().saturating_sub(PRESERVE_TURNS);
    let mut candidates: Vec<(usize, usize)> = (0..tail_end)
        .map(|i| {
            let turn_chars: usize = turns[i].iter().map(|(_, t)| t.len()).sum();
            (i, turn_chars)
        })
        .collect();

    // Trier par taille décroissante (évincer les plus gros d'abord).
    candidates.sort_by_key(|b| std::cmp::Reverse(b.1));

    let mut to_evict = std::collections::HashSet::new();
    let mut remaining_chars = current_chars;
    for (idx, turn_chars) in candidates {
        if remaining_chars <= target_chars {
            break;
        }
        to_evict.insert(idx);
        remaining_chars -= turn_chars;
    }

    // Reconstruire les messages.
    let mut compacted = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        if i < tail_end && to_evict.contains(&i) {
            continue;
        }
        compacted.extend(turn.iter().cloned());
    }

    tracing::debug!(
        new_chars = compacted.iter().map(|(_, t)| t.len()).sum::<usize>(),
        "compact_messages: terminée"
    );
    *messages = compacted;
}

/// Déroule un tour `session/prompt` avec boucle outil intégrée.
///
/// Refactor R1 : réécrit selon le pattern `GlmAcpAgent.prompt` + `runPromptLoop`.
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

    let span = tracing::info_span!(
        "turn",
        session = %session_id,
        chars_input = tracing::field::Empty,
        chars_output = tracing::field::Empty,
        tool_rounds = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    let _enter = span.enter();

    // Prompt serialization (bug de concurrence C — spec §4) : la sérialisation
    // est assurée par le handler `session/prompt` AVANT le spawn (wait_prompt_done
    // + set_prompt_handle), plus aucun self-wait ici.

    // begin_turn détecte les tours concurrents.
    let (session, mut cancel, generation) = match store.begin_turn(sid).await {
        Ok(triple) => triple,
        Err(TurnError::NotFound(_)) => {
            return responder.respond_with_error(
                AcpError::invalid_params()
                    .data(serde_json::json!({ "session_id": session_id.to_string() })),
            );
        }
        Err(TurnError::AlreadyRunning) => {
            return responder.respond_with_error(AcpError::invalid_params().data(
                serde_json::json!({
                    "session_id": session_id.to_string(),
                    "error": "a turn is already running; send session/cancel first"
                }),
            ));
        }
    };

    let mut guard = TurnGuard::new(store.clone(), sid.to_string(), session, generation);
    let session = guard.session_mut();

    let (user_text, images) = blocks_to_parts(&req.prompt);
    span.record("chars_input", user_text.chars().count());

    let message_id = MessageId::from(format!("msg_{}", uuid::Uuid::new_v4().simple()));

    // Titre auto-dérivé (inspiré de GlmAcpAgent.prompt titleUpdate).
    if session.title.is_none() && !user_text.trim().is_empty() {
        let title = derive_title(&user_text);
        session.title = Some(title.clone());
        safe_session_update(
            &cx,
            &session_id,
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
        );
    }

    // Uploads Scotty (images).
    let mut refs = Vec::new();
    if !images.is_empty() {
        let total = images.len();
        let upload_call_id = ToolCallId::from(format!("call_{}", uuid::Uuid::new_v4().simple()));
        safe_session_update(
            &cx,
            &session_id,
            SessionUpdate::ToolCall(
                ToolCall::new(
                    upload_call_id.clone(),
                    format!("Upload {total} image(s) (Scotty)"),
                )
                .kind(ToolKind::Fetch)
                .status(ToolCallStatus::InProgress),
            ),
        );

        for (idx, (b64, mime)) in images.iter().enumerate() {
            match client.upload_image(b64, mime).await {
                Ok(r) => {
                    tracing::info!(session = %session_id, r#ref = %r, "image uploadée (Scotty)");
                    refs.push(r);
                }
                Err(e) => {
                    safe_session_update(
                        &cx,
                        &session_id,
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            upload_call_id.clone(),
                            ToolCallUpdateFields::new()
                                .status(ToolCallStatus::Failed)
                                .content(vec![ToolCallContent::Content(
                                    agent_client_protocol::schema::v1::Content::new(
                                        ContentBlock::Text(TextContent::new(format!(
                                            "Upload image {}/{} échoué: {e:#}",
                                            idx + 1,
                                            total
                                        ))),
                                    ),
                                )]),
                        )),
                    );
                    span.record("outcome", "refusal_upload");
                    return responder.respond(PromptResponse::new(StopReason::Refusal));
                }
            }
        }
        safe_session_update(
            &cx,
            &session_id,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                upload_call_id,
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Content(
                        agent_client_protocol::schema::v1::Content::new(ContentBlock::Text(
                            TextContent::new(format!("{total} image(s) uploadée(s) avec succès")),
                        )),
                    )]),
            )),
        );
    }

    // Message utilisateur dans l'historique.
    session.messages.push((Role::User, user_text.clone()));

    // ----------------------------------------------------------------
    // Boucle outil : stream → parse → si tool_call → exécuter → re-prompt
    // ----------------------------------------------------------------
    let mut total_output = String::new();
    let cwd = session.cwd.clone();
    let additional_dirs = session.additional_directories.clone();
    let registry = &*tools;
    let mut tool_round = 0usize;
    let mut final_assistant_pushed = false;
    let mut overflow_retry_count = 0usize;

    // Créer le mode getter (thunk pour les changements mid-turn).
    let session_mode = session.mode;
    let mode_getter = || session_mode;

    for round in 0..MAX_TURNS {
        tool_round = round;

        // Vérifie annulation avant chaque sous-tour.
        if *cancel.borrow() {
            span.record("outcome", "cancelled");
            tracing::info!(session = %session_id, "tour annulé (session/cancel)");
            return responder.respond(PromptResponse::new(StopReason::Cancelled));
        }

        // Compaction proactive (inspiré de GlmAcpAgent.runPromptLoop).
        let history_chars: usize = session.messages.iter().map(|(_, t)| t.len()).sum();
        if history_chars > COMPACTION_THRESHOLD_CHARS {
            tracing::info!(
                session = %session_id,
                chars = history_chars,
                threshold = COMPACTION_THRESHOLD_CHARS,
                "compaction proactive déclenchée"
            );
            compact_messages(&mut session.messages, EMERGENCY_COMPACTION_CHARS);
        }

        let prompt = build_prompt(session, Some(registry));

        let mut rx = match client
            .stream(&prompt, &session.model, session.think, &refs)
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                let note = actionable_error_message(&e);

                // Retry sur erreur de contexte (inspiré de GlmAcpAgent ERR_CONTEXT_OVERFLOW).
                let is_overflow = e.to_string().contains("context")
                    || e.to_string().contains("too long")
                    || e.to_string().contains("tokens");

                if is_overflow && overflow_retry_count < 1 {
                    tracing::warn!(
                        session = %session_id,
                        "context overflow détecté, compaction d'urgence + retry"
                    );
                    compact_messages(&mut session.messages, EMERGENCY_COMPACTION_CHARS);
                    overflow_retry_count += 1;
                    continue; // Re-run the same turn.
                }

                if is_overflow {
                    emit_error_chunk(
                        &cx,
                        &session_id,
                        &message_id,
                        &format!("Context overflow persisted after emergency compaction: {e:#}"),
                    );
                    span.record("outcome", "refusal_start");
                    return responder.respond(PromptResponse::new(StopReason::MaxTokens));
                }

                // Erreur normale : surfacer comme [error] chunk (inspiré de glm-acp-agent).
                emit_error_chunk(&cx, &session_id, &message_id, &note);
                span.record("outcome", "failed_start");
                return responder.respond(PromptResponse::new(StopReason::EndTurn));
            }
        };

        // ThoughtSplitter.
        let is_thinking_model =
            gemini_acp_config::core::models::resolve(&session.model, gemini_acp_config::core::models::DEFAULT_MODEL)
                .map(|r| gemini_acp_config::core::models::is_thinking_mode(r.mode))
                .unwrap_or(false);
        let mut splitter = crate::thought::ThoughtSplitter::new(is_thinking_model);

        let mut assistant = String::new();
        let outcome = loop {
            tokio::select! {
                _ = cancel.changed() => break TurnOutcome::Cancelled,
                item = rx.recv() => {
                    let Some(item) = item else { break TurnOutcome::Complete };
                    match item {
                        Ok(delta) => {
                            assistant.push_str(&delta);
                            let (thought, message) = splitter.feed(&delta);
                            if !thought.is_empty() {
                                crate::thought::notify_thought(&cx, &session_id, &message_id, thought).await?;
                            }
                            if !message.is_empty() {
                                notify_text(&cx, &session_id, &message_id, message)?;
                            }
                        }
                        Err(e) => break TurnOutcome::Failed(e),
                    }
                }
            }
        };
        drop(rx);

        // Flush final du splitter.
        let (thought, message) = splitter.flush();
        if !thought.is_empty() {
            crate::thought::notify_thought(&cx, &session_id, &message_id, thought).await?;
        }
        if !message.is_empty() {
            notify_text(&cx, &session_id, &message_id, message)?;
        }

        if matches!(outcome, TurnOutcome::Cancelled) {
            span.record("outcome", "cancelled");
            return responder.respond(PromptResponse::new(StopReason::Cancelled));
        }

        if let TurnOutcome::Failed(e) = &outcome {
            // Surfacer l'erreur comme [error] chunk (inspiré de glm-acp-agent).
            emit_error_chunk(&cx, &session_id, &message_id, &actionable_stream_error(e));
            span.record("outcome", "failed");
            tracing::warn!(session = %session_id, "tour en échec: {e}");
            // Utiliser map_stop_reason pour déterminer si c'est un refusal.
            let stop = map_stop_reason_from_error(e);
            return responder.respond(PromptResponse::new(stop));
        }

        // Parse les tool_calls dans la réponse.
        let (clean_text, tool_calls) = parse_tool_calls(&assistant);

        if tool_calls.is_empty() || !session.tools_enabled || !registry.has_tools() {
            total_output = clean_text;
            break;
        }

        tracing::info!(
            session = %session_id,
            round = round,
            tool_count = tool_calls.len(),
            "tool calls détectés — exécution via ToolExecutor"
        );

        // Enregistrer la réponse assistant dans l'historique.
        let tool_blocks: String = tool_calls
            .iter()
            .map(|c| c.to_history_block())
            .collect::<Vec<_>>()
            .join("\n");
        let assistant_history = if clean_text.is_empty() {
            tool_blocks
        } else {
            format!("{}\n{}", clean_text, tool_blocks)
        };
        session.messages.push((Role::Assistant, assistant_history));

        // Exécuter chaque outil via ToolExecutor (inspiré de glm-acp-agent).
        let executor = ToolExecutor::new(
            &cx,
            &session_id,
            registry,
            &cwd,
            &additional_dirs,
            &mode_getter,
        );

        for call in &tool_calls {
            if *cancel.borrow() {
                span.record("outcome", "cancelled");
                return responder.respond(PromptResponse::new(StopReason::Cancelled));
            }

            let result = executor.execute(&call.name, &call.arguments).await;

            session.messages.push((
                Role::Tool,
                gemini_acp_runtime::tools::prompt::format_tool_result(&call.name, &result.content),
            ));
        }

        if round == MAX_TURNS - 1 {
            tracing::warn!(
                session = %session_id,
                rounds = MAX_TURNS,
                "limite de boucle outil atteinte"
            );
            total_output = "[Limite d'itérations outil atteinte]".into();
            final_assistant_pushed = true;
            break;
        }
    }

    span.record("tool_rounds", tool_round);
    span.record("chars_output", total_output.chars().count());

    // Finalisation.
    if !final_assistant_pushed && !total_output.trim().is_empty() {
        session
            .messages
            .push((Role::Assistant, total_output.clone()));
    }

    // Usage (best-effort).
    if let Err(e) = notify_usage(
        &cx,
        &session_id,
        &build_prompt(session, Some(registry)),
        &total_output,
    ) {
        tracing::warn!(session = %session_id, "notify_usage a échoué: {e}");
    }

    // Fin normale du tour.
    guard.finish().await;
    span.record("outcome", "end_turn");

    // Réponse avec stop reason mappé (inspiré de GlmAcpAgent).
    responder.respond(PromptResponse::new(StopReason::EndTurn))
}
