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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenv::dotenv() {
        eprintln!("WARNING: .env load failed: {}", e);
    }
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    tracing::info!(
        "kalshi-bot v2 daemon | paper_trade={} confirm_live={} tp={}¢ sl={}¢",
        config.paper_trade, config.confirm_live,
        config.tp_cents_per_share, config.sl_cents_per_share
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

    // Binance WebSocket
    let (binance_tx, mut binance_rx) = tokio::sync::mpsc::channel::<binance_ws::BtcPriceUpdate>(256);
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

    // Track latest BTC price
    let mut latest_btc_price: Option<f64> = None;
    // Track current subscribed ticker for WS
    let mut subscribed_ticker: Option<String> = None;

    // Run the first entry cycle immediately
    tracing::info!("Running initial entry cycle");
    if let Err(e) = engine::entry_cycle(&exchange, &brain, &price_feed, &config, &position_mgr).await {
        tracing::error!("Initial entry cycle error: {}", e);
    }

    tracing::info!("Entering event loop");
    loop {
        // If position_mgr got a position from entry_cycle, subscribe to its ticker
        if let Some(pos) = position_mgr.position() {
            if subscribed_ticker.as_deref() != Some(&pos.ticker) {
                let ticker = pos.ticker.clone();
                kalshi_ws_sender.subscribe(
                    vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                    &ticker,
                ).await;
                subscribed_ticker = Some(ticker);
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
                        position_mgr.on_fill(&fill);

                        // Subscribe to orderbook for the filled ticker if needed
                        if subscribed_ticker.as_deref() != Some(&fill.ticker) {
                            let ticker = fill.ticker.clone();
                            kalshi_ws_sender.subscribe(
                                vec!["orderbook_delta".into(), "market_lifecycle_v2".into()],
                                &ticker,
                            ).await;
                            subscribed_ticker = Some(ticker);
                        }
                    }
                    KalshiWsEvent::MarketLifecycle(lifecycle) => {
                        tracing::info!(
                            "Market lifecycle: {} status={} result={:?}",
                            lifecycle.ticker, lifecycle.status, lifecycle.result
                        );
                        // If market settled while we hold a position, clear it
                        if lifecycle.status == "settled" || lifecycle.status == "finalized" {
                            if position_mgr.has_position() {
                                if let Some(pos) = position_mgr.position() {
                                    if pos.ticker == lifecycle.ticker {
                                        tracing::info!("Market settled — clearing position");
                                        position_mgr.clear_position();
                                        // Unsubscribe
                                        if let Some(ref ticker) = subscribed_ticker {
                                            kalshi_ws_sender.unsubscribe(
                                                vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                                                ticker,
                                            ).await;
                                        }
                                        subscribed_ticker = None;
                                    }
                                }
                            }
                        }
                    }
                    KalshiWsEvent::Disconnected => {
                        tracing::warn!("Kalshi WS disconnected — will auto-reconnect");
                        // Re-subscribe after reconnect
                        if let Some(ref ticker) = subscribed_ticker {
                            kalshi_ws_sender.subscribe(
                                vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                                ticker,
                            ).await;
                        }
                    }
                }
            }

            Some(update) = binance_rx.recv() => {
                latest_btc_price = Some(update.price);
                tracing::debug!("BTC price: ${:.2}", update.price);
            }

            _ = entry_timer.tick() => {
                tracing::info!("Entry cycle tick (btc=${:.2})", latest_btc_price.unwrap_or(0.0));
                if let Err(e) = engine::entry_cycle(
                    &exchange, &brain, &price_feed, &config, &position_mgr
                ).await {
                    tracing::error!("Entry cycle error: {}", e);
                }
            }

            _ = position_timer.tick() => {
                if position_mgr.has_position() {
                    if let Some(pnl) = position_mgr.unrealized_pnl_per_share() {
                        tracing::debug!("Position check: unrealized P&L = {}¢/share", pnl);
                    }
                    if let Some(reason) = position_mgr.check_exit() {
                        tracing::info!("Exit signal: {:?}", reason);
                        if let Err(e) = engine::execute_exit(
                            &exchange, &mut position_mgr, reason, &config
                        ).await {
                            tracing::error!("Exit execution error: {}", e);
                        }
                        // Unsubscribe from old ticker
                        if let Some(ref ticker) = subscribed_ticker {
                            kalshi_ws_sender.unsubscribe(
                                vec!["orderbook_delta".into(), "fill".into(), "market_lifecycle_v2".into()],
                                ticker,
                            ).await;
                        }
                        subscribed_ticker = None;
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
