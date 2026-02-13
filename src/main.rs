mod adapters;
mod core;
mod ports;
mod safety;
mod storage;

use adapters::binance::BinanceClient;
use adapters::binance_ws;
use adapters::kalshi::client::KalshiClient;
use adapters::kalshi::websocket::{self as kalshi_ws, KalshiWsEvent};
use adapters::openrouter::OpenRouterClient;
use core::engine;
use core::position_manager::PositionManager;
use core::types::Config;
use std::collections::{HashMap, HashSet};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenv::dotenv() {
        eprintln!("WARNING: .env load failed: {}", e);
    }
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    tracing::info!(
        "kalshi-bot v2 daemon | paper_trade={} confirm_live={} tp={}¢ sl={}¢ assets={:?}",
        config.paper_trade, config.confirm_live,
        config.tp_cents_per_share, config.sl_cents_per_share,
        config.series_tickers
    );

    safety::validate_startup(&config)?;

    let exchange = KalshiClient::new(&config)?;
    let brain = OpenRouterClient::new(&config)?;
    let price_feed = BinanceClient::new(&config)?;

    let mut position_mgr = PositionManager::new(&config);
    let mut shutdown_rx = safety::setup_signal_handler();

    // Kalshi WebSocket
    let (kalshi_tx, mut kalshi_rx) = tokio::sync::mpsc::channel::<KalshiWsEvent>(256);
    let kalshi_auth = adapters::kalshi::auth::KalshiAuth::new(
        config.kalshi_key_id.clone(),
        &config.kalshi_private_key_pem,
    )?;
    let kalshi_ws_sender = kalshi_ws::connect(&config.kalshi_ws_url, &kalshi_auth, kalshi_tx).await?;

    // Binance WebSocket — combined stream for all assets
    let (binance_tx, mut binance_rx) = tokio::sync::mpsc::channel::<binance_ws::CryptoPriceUpdate>(256);
    let binance_ws_url = config.binance_ws_url.clone();
    tokio::spawn(async move {
        if let Err(e) = binance_ws::connect(&binance_ws_url, binance_tx).await {
            tracing::error!("Binance WS fatal: {}", e);
        }
    });

    // Timers
    let mut entry_timer = tokio::time::interval(
        std::time::Duration::from_secs(config.entry_cycle_interval_secs),
    );
    let mut position_timer = tokio::time::interval(
        std::time::Duration::from_secs(config.position_check_interval_secs),
    );

    // Track latest prices per Binance symbol (e.g., "BTCUSDT" → 66322.01)
    let mut latest_prices: HashMap<String, f64> = HashMap::new();
    // Track subscribed market tickers for WS
    let mut subscribed_tickers: HashSet<String> = HashSet::new();

    // Run initial entry cycles for all series
    tracing::info!("Running initial entry cycles for {} assets", config.series_tickers.len());
    for series in &config.series_tickers {
        if let Err(e) = engine::entry_cycle(
            &exchange, &brain, &price_feed, &config, &position_mgr, series
        ).await {
            tracing::error!("[{}] Initial entry cycle error: {}", series, e);
        }
    }

    tracing::info!("Entering event loop");
    loop {
        // Subscribe to orderbook/fill/lifecycle for any new position tickers
        for ticker in position_mgr.position_tickers() {
            if !subscribed_tickers.contains(&ticker) {
                kalshi_ws_sender.subscribe(
                    vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                    &ticker,
                ).await;
                subscribed_tickers.insert(ticker);
            }
        }

        tokio::select! {
            Some(event) = kalshi_rx.recv() => {
                match event {
                    KalshiWsEvent::Orderbook(update) => {
                        tracing::debug!(
                            "Orderbook update: {} yes_levels={} no_levels={}",
                            update.ticker, update.yes.len(), update.no.len()
                        );
                        position_mgr.on_orderbook_update(update);
                    }
                    KalshiWsEvent::Fill(fill) => {
                        tracing::info!(
                            "Fill: {:?} {}x @ {}¢ on {} (order {})",
                            fill.side, fill.shares, fill.price_cents,
                            fill.ticker, fill.order_id
                        );
                        let ticker = fill.ticker.clone();
                        position_mgr.on_fill(&fill);

                        // Subscribe to orderbook for the filled ticker
                        if !subscribed_tickers.contains(&ticker) {
                            kalshi_ws_sender.subscribe(
                                vec!["orderbook_delta".into(), "market_lifecycle_v2".into()],
                                &ticker,
                            ).await;
                            subscribed_tickers.insert(ticker);
                        }
                    }
                    KalshiWsEvent::MarketLifecycle(lifecycle) => {
                        tracing::info!(
                            "Market lifecycle: {} status={} result={:?}",
                            lifecycle.ticker, lifecycle.status, lifecycle.result
                        );
                        if lifecycle.status == "settled" || lifecycle.status == "finalized" {
                            if position_mgr.position_for_ticker(&lifecycle.ticker).is_some() {
                                tracing::info!("Market settled — clearing position on {}", lifecycle.ticker);
                                position_mgr.clear_position(&lifecycle.ticker);
                                // Unsubscribe
                                kalshi_ws_sender.unsubscribe(
                                    vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                                    &lifecycle.ticker,
                                ).await;
                                subscribed_tickers.remove(&lifecycle.ticker);
                            }
                        }
                    }
                    KalshiWsEvent::Disconnected => {
                        tracing::warn!("Kalshi WS disconnected — will auto-reconnect");
                        // Re-subscribe all active tickers after reconnect
                        for ticker in &subscribed_tickers {
                            kalshi_ws_sender.subscribe(
                                vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                                ticker,
                            ).await;
                        }
                    }
                }
            }

            Some(update) = binance_rx.recv() => {
                tracing::debug!("{} price: ${:.2}", update.symbol, update.price);
                latest_prices.insert(update.symbol, update.price);
            }

            _ = entry_timer.tick() => {
                let price_summary: Vec<String> = latest_prices.iter()
                    .map(|(s, p)| format!("{}=${:.2}", s, p))
                    .collect();
                tracing::info!(
                    "Entry cycle tick | {} positions | prices: {}",
                    position_mgr.position_count(),
                    if price_summary.is_empty() { "none".into() } else { price_summary.join(", ") }
                );

                // Run entry cycle for each series that doesn't have a position
                for series in &config.series_tickers {
                    if let Err(e) = engine::entry_cycle(
                        &exchange, &brain, &price_feed, &config, &position_mgr, series
                    ).await {
                        tracing::error!("[{}] Entry cycle error: {}", series, e);
                    }
                }
            }

            _ = position_timer.tick() => {
                if position_mgr.position_count() > 0 {
                    // Log unrealized P&L for all positions
                    for ticker in position_mgr.position_tickers() {
                        if let Some(pnl) = position_mgr.unrealized_pnl_per_share(&ticker) {
                            tracing::debug!("Position {}: unrealized P&L = {}¢/share", ticker, pnl);
                        }
                    }

                    // Check all positions for TP/SL exits
                    let exits = position_mgr.check_exits();
                    for (ticker, reason) in exits {
                        tracing::info!("Exit signal: {:?} on {}", reason, ticker);
                        if let Err(e) = engine::execute_exit(
                            &exchange, &mut position_mgr, &ticker, reason, &config
                        ).await {
                            tracing::error!("Exit execution error on {}: {}", ticker, e);
                        }
                        // Unsubscribe from exited ticker
                        kalshi_ws_sender.unsubscribe(
                            vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                            &ticker,
                        ).await;
                        subscribed_tickers.remove(&ticker);
                    }
                }
            }

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("Shutdown signal received — exiting event loop");
                    break;
                }
            }
        }
    }

    tracing::info!("kalshi-bot v2 daemon stopped");
    Ok(())
}
