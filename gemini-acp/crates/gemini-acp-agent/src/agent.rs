//! Construction de l'agent ACP et câblage du transport stdio.
//!
//! Refactor R1 — inspiré de `glm-acp-agent/src/protocol/agent.ts` :
//!
//! - **Fork session** : ajout du handler `session/fork` (inspiré de
//!   `GlmAcpAgent.unstable_forkSession`).
//! - **Set session mode** : ajout du handler `session/set_mode` (inspiré de
//!   `GlmAcpAgent.setSessionMode` avec les 3 modes : default, accept_edits,
//!   bypass_permissions).
//! - **Capabilities enrichies** : annonce `fork` et `mcpCapabilities`.
//! - **Prompt serialization** : le handler prompt attend le prompt précédent
//!   via `store.wait_prompt_done` avant de s'enregistrer (bug de concurrence C
//!   — spec §4).
//! - **Interactive tool context** : chaque tour reçoit un contexte ACP task-local
//!   pour les outils comme `AskUserQuestion`, isolé par tâche/session.
//!
//! Refactor 3-crates (spec §5.2) : ce crate ne possède plus son propre
//! `AppState` — il réutilise directement `gemini_acp_runtime::AppState`,
//! construit par `gemini_acp_runtime::AgentRuntime::from_config`.

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Agent, Error as AcpError, Stdio};

use gemini_acp_runtime::AppState;

use crate::handlers;
use crate::prompt;

/// Construit l'agent ACP et le lance sur le transport stdio.
///
/// Refactor R1 : ajout des handlers fork et set_mode.
pub async fn run_agent(state: AppState) -> Result<(), AcpError> {
    let h_store = state.store.clone();
    let h_client = state.client.clone();
    let h_tools = state.tools.clone();

    Agent
        .builder()
        .name("gemini-acp")
        // initialize -------------------------------------------------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: InitializeRequest, responder, _cx| {
                    handlers::init::handle(req, responder, &state).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/new ------------------------------------------------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: NewSessionRequest, responder, _cx| {
                    handlers::session::handle_new(req, responder, &state).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/prompt (délégué à une tâche, spec §3.1) ------------------
        // Sérialisation des prompts (bug de concurrence C — spec §4) :
        // 1) attendre la fin du tour précédent AVANT de s'enregistrer ;
        // 2) enregistrer le handle AVANT le spawn (c'est le signal que le
        //    PROCHAIN prompt attendra) — le `busy` de `begin_turn` reste le
        //    filet de sécurité pour les prompts réellement simultanés.
        .on_receive_request(
            {
                let store = h_store.clone();
                let client = h_client.clone();
                let tools = h_tools.clone();
                async move |req: PromptRequest, responder, cx| {
                    let turn_cx = cx.clone();
                    let store = store.clone();
                    let client = client.clone();
                    let tools = tools.clone();
                    let sid = req.session_id.0.clone();
                    let session_id = req.session_id.clone();
                    let store_for_handle = store.clone();

                    // 1) Attendre la fin du tour précédent AVANT de s'enregistrer.
                    store_for_handle.wait_prompt_done(&sid).await;

                    // 2) Enregistrer le handle du tour courant AVANT le spawn :
                    //    c'est le signal que le PROCHAIN prompt attendra.
                    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
                    store_for_handle.set_prompt_handle(&sid, done_rx).await;

                    let _ = cx.spawn(async move {
                        let interactive = gemini_acp_runtime::tools::interactive::InteractiveContext {
                            cx: turn_cx.clone(),
                            session_id,
                        };
                        let result = gemini_acp_runtime::tools::interactive::scope(
                            interactive,
                            async move {
                                prompt::run_turn(store, tools, client, req, responder, turn_cx).await
                            },
                        )
                        .await;
                        let _ = done_tx.send(());
                        result
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/list -----------------------------------------------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: ListSessionsRequest, responder, _cx| {
                    handlers::session::handle_list(req, responder, &state).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/load (rejeu de l'historique AVANT la réponse) ------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: LoadSessionRequest, responder, cx| {
                    handlers::session::handle_load(req, responder, &state, &cx).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/resume (restauration sans rejeu) -------------------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: ResumeSessionRequest, responder, cx| {
                    handlers::session::handle_resume(req, responder, &state, &cx).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/delete ---------------------------------------------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: DeleteSessionRequest, responder, _cx| {
                    handlers::session::handle_delete(req, responder, &state).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/close (annule + libère, fichier conservé) ----------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: CloseSessionRequest, responder, _cx| {
                    handlers::session::handle_close(req, responder, &state).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/set_config_option (émets ConfigOptionUpdate) -------------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: SetSessionConfigOptionRequest, responder, cx| {
                    handlers::config::handle(req, responder, &state, &cx).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/set_mode (R1: inspiré de GlmAcpAgent.setSessionMode) -----
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: SetSessionModeRequest, responder, cx| {
                    handlers::session::handle_set_mode(req, responder, &state, &cx).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // session/fork (R1: inspiré de GlmAcpAgent.unstable_forkSession) -------
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: ForkSessionRequest, responder, _cx| {
                    handlers::session::handle_fork(req, responder, &state).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // notification session/cancel --------------------------------------
        .on_receive_notification(
            {
                let state = state.clone();
                async move |notif: CancelNotification, _cx| {
                    handlers::cancel::handle(notif, &state).await
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}