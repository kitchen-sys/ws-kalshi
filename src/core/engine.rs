use crate::core::position_manager::PositionManager;
use crate::core::{indicators, risk, stats, types::*};
use crate::ports::brain::Brain;
use crate::ports::exchange::Exchange;
use crate::ports::price_feed::PriceFeed;
use crate::storage;
use anyhow::Result;

/// Run an entry cycle for a specific series (e.g., "KXBTC15M").
/// Skips if we already hold a position for this series.
pub async fn entry_cycle(
    exchange: &dyn Exchange,
    brain: &dyn Brain,
    price_feed: &dyn PriceFeed,
    config: &Config,
    position_mgr: &PositionManager,
    series_ticker: &str,
) -> Result<()> {
    let asset = series_to_asset_label(series_ticker);

    // Skip entry if we already hold a position for this series
    if position_mgr.has_position_for_series(series_ticker) {
        tracing::info!("[{}] Holding position — skipping entry cycle", asset);
        return Ok(());
    }

    // 1. CANCEL stale resting orders from previous cycles
    let resting = exchange.resting_orders().await?;
    for order in &resting {
        exchange.cancel_order(&order.order_id).await?;
        storage::cancel_trade(&order.order_id)?;
        tracing::info!("[{}] Canceled stale order: {}", asset, order.order_id);
    }

    // 2. SETTLE — check if previous trade settled, update ledger + stats
    let mut ledger = storage::read_ledger()?;
    if let Some(pending) = ledger.iter().rev().find(|r| r.result == "pending") {
        let pending_ticker = pending.ticker.clone();
        let pending_timestamp = pending.timestamp.clone();
        let settlements = exchange.settlements(&pending_ticker).await?;
        if let Some(s) = settlements.first() {
            storage::settle_last_trade(s)?;
            ledger = storage::read_ledger()?;
            let settled_stats = stats::compute(&ledger);
            storage::write_stats(&settled_stats)?;
            tracing::info!(
                "[{}] Settled: {} (market_result={}) | {} {}¢",
                asset, s.result.to_uppercase(), s.market_result, s.ticker, s.pnl_cents
            );
        } else {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&pending_timestamp) {
                let age_min = (chrono::Utc::now() - ts.with_timezone(&chrono::Utc)).num_minutes();
                if age_min > 30 {
                    let zombie = Settlement {
                        ticker: pending_ticker.clone(),
                        side: Side::Yes,
                        count: 0,
                        price_cents: 0,
                        result: "unknown".into(),
                        pnl_cents: 0,
                        settled_time: chrono::Utc::now().to_rfc3339(),
                        market_result: "unknown".into(),
                    };
                    storage::settle_last_trade(&zombie)?;
                    ledger = storage::read_ledger()?;
                    tracing::warn!(
                        "[{}] Zombie cleanup: pending entry for {} was {}min old",
                        asset, pending_ticker, age_min
                    );
                }
            }
        }
    }

    // 3. RISK
    let computed_stats = stats::compute(&ledger);
    let balance = exchange.balance().await?;

    if let Some(veto) = risk::check(&computed_stats, balance, config) {
        tracing::info!("[{}] Risk veto: {}", asset, veto);
        return Ok(());
    }

    // 4. MARKET — fetch active market for this series
    let market = match exchange.active_market(series_ticker).await? {
        Some(m) if m.minutes_to_expiry >= config.min_minutes_to_expiry => m,
        Some(m) => {
            tracing::info!("[{}] Too close to expiry: {:.1}min", asset, m.minutes_to_expiry);
            return Ok(());
        }
        None => {
            tracing::info!("[{}] No active market", asset);
            return Ok(());
        }
    };

    // 5. ORDERBOOK
    let orderbook = exchange.orderbook(&market.ticker).await?;

    // 5.5. CRYPTO PRICE — fetch for the relevant asset
    let binance_symbol = series_to_binance_symbol(series_ticker);
    let crypto_price = fetch_crypto_price(price_feed, binance_symbol).await;

    // 6. BRAIN
    let context = DecisionContext {
        prompt_md: storage::read_prompt()?,
        stats: computed_stats,
        last_n_trades: ledger.iter().rev().take(20).cloned().collect(),
        market: market.clone(),
        orderbook,
        crypto_price,
        crypto_label: format!("{} (Binance {})", asset, binance_symbol),
    };

    let decision = brain.decide(&context).await?;

    // 7. VALIDATE
    if decision.action == Action::Pass {
        tracing::info!("[{}] PASS: {}", asset, decision.reasoning);
        return Ok(());
    }

    let side = decision.side.unwrap_or(Side::Yes);
    let shares = decision.shares.unwrap_or(1).min(config.max_shares);
    let price = decision.max_price_cents.unwrap_or(50).clamp(1, 99);

    // 8. FINAL POSITION CHECK
    let fresh_positions = exchange.positions().await?;
    if fresh_positions.iter().any(|p| p.ticker == market.ticker) {
        tracing::warn!("[{}] Position on {} — aborting order", asset, market.ticker);
        return Ok(());
    }

    // 9. EXECUTE
    let current_stats = stats::compute(&ledger);

    if config.paper_trade {
        let paper_id = format!("paper-{}", chrono::Utc::now().timestamp_millis());
        tracing::info!(
            "[{}] PAPER: {:?} {}x @ {}¢ | {} ({})",
            asset, side, shares, price, market.ticker, paper_id
        );
        storage::append_ledger(&LedgerRow {
            timestamp: chrono::Utc::now().to_rfc3339(),
            ticker: market.ticker.clone(),
            side: format!("{:?}", side).to_lowercase(),
            shares,
            price,
            result: "pending".into(),
            pnl_cents: 0,
            cumulative_cents: current_stats.total_pnl_cents,
            order_id: paper_id,
        })?;
    } else {
        let order_result = exchange
            .place_order(&OrderRequest {
                ticker: market.ticker.clone(),
                side: side.clone(),
                shares,
                price_cents: price,
            })
            .await;

        match order_result {
            Ok(result) => {
                tracing::info!(
                    "[{}] LIVE: {:?} {}x @ {}¢ | {} (order {} status: {})",
                    asset, side, shares, price, market.ticker, result.order_id, result.status
                );
                if let Err(e) = storage::append_ledger(&LedgerRow {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    ticker: market.ticker.clone(),
                    side: format!("{:?}", side).to_lowercase(),
                    shares,
                    price,
                    result: "pending".into(),
                    pnl_cents: 0,
                    cumulative_cents: current_stats.total_pnl_cents,
                    order_id: result.order_id.clone(),
                }) {
                    tracing::error!(
                        "CRITICAL: Order {} placed but ledger write failed: {}",
                        result.order_id, e
                    );
                    return Err(e.into());
                }
            }
            Err(e) => {
                tracing::error!("[{}] Order placement failed: {}", asset, e);
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Execute an early exit (TP/SL sell) for a specific position by market ticker.
pub async fn execute_exit(
    exchange: &dyn Exchange,
    position_mgr: &mut PositionManager,
    ticker: &str,
    reason: ExitReason,
    config: &Config,
) -> Result<()> {
    let exit_event = match position_mgr.build_exit_event(ticker, reason.clone()) {
        Some(e) => e,
        None => {
            tracing::warn!("Cannot build exit event for {} — no position or orderbook", ticker);
            return Ok(());
        }
    };

    let exit_order = match position_mgr.build_exit_order(ticker) {
        Some(o) => o,
        None => {
            tracing::warn!("Cannot build exit order for {} — no position or orderbook", ticker);
            return Ok(());
        }
    };

    tracing::info!(
        "EXIT {}: {:?} {}x | entry={}¢ exit={}¢ pnl={}¢ on {}",
        reason, exit_order.side, exit_order.shares,
        exit_event.entry_price_cents, exit_event.exit_price_cents,
        exit_event.pnl_cents, ticker
    );

    if config.paper_trade {
        tracing::info!("PAPER EXIT: {} on {}", reason, ticker);
    } else {
        match exchange.sell_order(&exit_order).await {
            Ok(result) => {
                tracing::info!("Sell order placed: {} status={}", result.order_id, result.status);
            }
            Err(e) => {
                tracing::error!("Sell order failed on {}: {}", ticker, e);
                return Err(e);
            }
        }
    }

    if let Err(e) = storage::record_early_exit(&exit_event) {
        tracing::error!("Failed to record early exit in ledger: {}", e);
    }

    let ledger = storage::read_ledger()?;
    let updated_stats = stats::compute(&ledger);
    storage::write_stats(&updated_stats)?;

    position_mgr.clear_position(ticker);
    Ok(())
}

async fn fetch_crypto_price(price_feed: &dyn PriceFeed, symbol: &str) -> Option<PriceSnapshot> {
    let (candles_1m, candles_5m, spot) = tokio::join!(
        price_feed.candles(symbol, "1m", 15),
        price_feed.candles(symbol, "5m", 12),
        price_feed.spot_price(symbol),
    );

    let candles_1m = candles_1m.ok().flatten()?;
    let candles_5m = candles_5m.ok().flatten()?;
    let spot = spot.ok().flatten()?;

    if candles_1m.is_empty() {
        tracing::warn!("Binance returned empty 1m candles for {}", symbol);
        return None;
    }

    let ind = indicators::compute(&candles_1m, &candles_5m, spot);

    Some(PriceSnapshot {
        candles_1m,
        candles_5m,
        spot_price: spot,
        indicators: ind,
    })
}
