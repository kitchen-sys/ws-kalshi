use crate::core::types::Config;
use crate::storage;
use tokio::sync::watch;

pub fn validate_startup(config: &Config) -> anyhow::Result<()> {
    if config.kalshi_private_key_pem.is_empty() {
        anyhow::bail!("KALSHI_PRIVATE_KEY_PATH is empty or file not found");
    }
    if !config.kalshi_private_key_pem.contains("BEGIN") {
        anyhow::bail!("PEM file doesn't look like a private key");
    }

    if config.series_tickers.is_empty() {
        anyhow::bail!("KALSHI_SERIES_TICKERS not set — run discovery first");
    }

    if config.openrouter_api_key.is_empty() {
        anyhow::bail!("OPENROUTER_API_KEY not set");
    }
    if config.kalshi_key_id.is_empty() {
        anyhow::bail!("KALSHI_API_KEY_ID not set");
    }

    if !std::path::Path::new("brain/ledger.md").exists() {
        anyhow::bail!("brain/ledger.md not found");
    }
    storage::read_ledger()?;

    if !std::path::Path::new("brain/prompt.md").exists() {
        anyhow::bail!("brain/prompt.md not found");
    }

    if !config.paper_trade && !config.confirm_live {
        anyhow::bail!(
            "PAPER_TRADE=false but CONFIRM_LIVE is not true. \
             Set CONFIRM_LIVE=true to acknowledge real money trading."
        );
    }

    if !config.paper_trade {
        tracing::warn!("LIVE TRADING ENABLED — real money at risk");
    }

    Ok(())
}

/// Set up a signal handler for graceful shutdown (SIGINT, SIGTERM).
/// Returns a watch receiver that becomes `true` when shutdown is requested.
pub fn setup_signal_handler() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => {
                    tracing::info!("Received SIGINT — shutting down gracefully");
                }
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM — shutting down gracefully");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("Received Ctrl+C — shutting down gracefully");
        }
        let _ = tx.send(true);
    });
    rx
}
