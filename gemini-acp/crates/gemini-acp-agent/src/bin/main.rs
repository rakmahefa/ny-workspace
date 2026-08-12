//! Binaire `gemini-acp` : transport ACP stdio vers l'agent Gemini.
//!
//! Responsabilités volontairement minimales :
//! - initialiser le logging sur stderr ;
//! - résoudre la configuration (`gemini_acp_config::AgentConfig`) ;
//! - créer le runtime (`gemini_acp_runtime::AgentRuntime`) ;
//! - lancer le transport ACP (`gemini_acp_agent::run_agent`) et gérer le
//!   signal d'arrêt.
//!
//! Le protocole et les handlers vivent dans `agent.rs` et `handlers/`.

use anyhow::{Context, Result};

use gemini_acp_agent::run_agent;
use gemini_acp_config::AgentConfig;
use gemini_acp_runtime::AgentRuntime;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = AgentConfig::from_env();
    let runtime = AgentRuntime::from_config(config).await?;

    tokio::select! {
        result = run_agent(runtime.state().clone()) => {
            result.context("transport ACP arrêté avec une erreur")?;
        }
        _ = wait_for_shutdown_signal() => {
            runtime.shutdown().await;
            tracing::info!("shutdown gracieux terminé");
        }
    }

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::error!(%error, "installation du handler SIGTERM impossible");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::error!(%error, "installation du handler SIGINT impossible");
                return;
            }
        };

        tokio::select! {
            _ = sigterm.recv() => tracing::info!("SIGTERM reçu"),
            _ = sigint.recv() => tracing::info!("SIGINT reçu"),
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "installation du handler Ctrl-C impossible");
        } else {
            tracing::info!("Ctrl-C reçu");
        }
    }
}
