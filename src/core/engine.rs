use crate::core::position_manager::PositionManager;
use crate::core::{indicators, risk, stats, types::*};
use crate::ports::brain::Brain;
use crate::ports::exchange::Exchange;
use crate::ports::price_feed::PriceFeed;
use crate::storage;
use anyhow::Result;

pub async fn entry_cycle(
    exchange: &dyn Exchange,
    brain: &dyn Brain,
    price_feed: &dyn PriceFeed,
    config: &Config,
    position_mgr: &PositionManager,
) -> Result<()> {
    // Skip entry if we already hold a position
    if position_mgr.has_position() {
        tracing::info!(
            "Holding position on {} — skipping entry cycle",
            position_mgr.position().unwrap().ticker
        );
        return Ok(());
    }

    // 1. CANCEL stale resting orders from previous cycles
    let resting = exchange.resting_orders().await?;
    for order in &resting {
        exchange.cancel_order(&order.order_id).await?;
        storage::cancel_trade(&order.order_id)?;
        tracing::info!("Canceled stale order: {} (ledger marked cancelled)", order.order_id);
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
                "Settled: {} (market_result={}) | {} {}¢",
                s.result.to_uppercase(), s.market_result, s.ticker, s.pnl_cents
            );
        } else {
            // No settlement found — check if pending entry is stale (>30 min old)
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
                        "Zombie cleanup: pending entry for {} was {}min old, marked unknown",
                        pending_ticker, age_min
                    );
                }
            }
        }
    }

    // 3. RISK — deterministic checks in Rust
    let computed_stats = stats::compute(&ledger);
    let balance = exchange.balance().await?;

    if let Some(veto) = risk::check(&computed_stats, balance, config) {
        tracing::info!("Risk veto: {}", veto);
        return Ok(());
    }

    // 4. MARKET — fetch by hardcoded series ticker
    let market = match exchange.active_market().await? {
        Some(m) if m.minutes_to_expiry >= config.min_minutes_to_expiry => m,
        Some(m) => {
            tracing::info!("Too close to expiry: {:.1}min", m.minutes_to_expiry);
            return Ok(());
        }
        None => {
            tracing::info!("No active market");
            return Ok(());
        }
    };

    // 5. ORDERBOOK
    let orderbook = exchange.orderbook(&market.ticker).await?;

    // 5.5. BTC PRICE — external reference (best-effort, non-blocking)
    let btc_price = fetch_btc_price(price_feed).await;

    // 6. BRAIN — one AI call
    let context = DecisionContext {
        prompt_md: storage::read_prompt()?,
        stats: computed_stats,
        last_n_trades: ledger.iter().rev().take(20).cloned().collect(),
        market: market.clone(),
        orderbook,
        btc_price,
    };

    let decision = brain.decide(&context).await?;

    // 7. VALIDATE
    if decision.action == Action::Pass {
        tracing::info!("PASS: {}", decision.reasoning);
        return Ok(());
    }

    let side = decision.side.unwrap_or(Side::Yes);
    let shares = decision.shares.unwrap_or(1).min(config.max_shares);
    let price = decision.max_price_cents.unwrap_or(50).clamp(1, 99);

    // 8. FINAL POSITION CHECK — only block if position is on THIS market
    let fresh_positions = exchange.positions().await?;
    if fresh_positions.iter().any(|p| p.ticker == market.ticker) {
        tracing::warn!("Position on {} — aborting order", market.ticker);
        return Ok(());
    }

    // 9. EXECUTE — order FIRST, ledger SECOND
    let current_stats = stats::compute(&ledger);

    if config.paper_trade {
        let paper_id = format!("paper-{}", chrono::Utc::now().timestamp_millis());
        tracing::info!(
            "PAPER: {:?} {}x @ {}¢ | {} ({})",
            side,
            shares,
            price,
            market.ticker,
            paper_id
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
                    "LIVE: {:?} {}x @ {}¢ | {} (order {} status: {})",
                    side, shares, price, market.ticker, result.order_id, result.status
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
                        result.order_id,
                        e
                    );
                    return Err(e.into());
                }
            }
            Err(e) => {
                tracing::error!("Order placement failed: {}", e);
                return Err(e);
            }
        }
    }

    // 10. EXIT
    Ok(())
}

/// Execute an early exit (TP/SL sell) for the current position.
pub async fn execute_exit(
    exchange: &dyn Exchange,
    position_mgr: &mut PositionManager,
    reason: ExitReason,
    config: &Config,
) -> Result<()> {
    let exit_event = match position_mgr.build_exit_event(reason.clone()) {
        Some(e) => e,
        None => {
            tracing::warn!("Cannot build exit event — no position or orderbook");
            return Ok(());
        }
    };

    let exit_order = match position_mgr.build_exit_order() {
        Some(o) => o,
        None => {
            tracing::warn!("Cannot build exit order — no position or orderbook");
            return Ok(());
        }
    };

    tracing::info!(
        "EXIT {}: {:?} {}x | entry={}¢ exit={}¢ pnl={}¢ on {}",
        reason,
        exit_order.side,
        exit_order.shares,
        exit_event.entry_price_cents,
        exit_event.exit_price_cents,
        exit_event.pnl_cents,
        exit_event.ticker
    );

    if config.paper_trade {
        tracing::info!("PAPER EXIT: {} on {}", reason, exit_event.ticker);
    } else {
        match exchange.sell_order(&exit_order).await {
            Ok(result) => {
                tracing::info!(
                    "Sell order placed: {} status={}",
                    result.order_id,
                    result.status
                );
            }
            Err(e) => {
                tracing::error!("Sell order failed: {}", e);
                return Err(e);
            }
        }
    }

    // Record the early exit in ledger
    if let Err(e) = storage::record_early_exit(&exit_event) {
        tracing::error!("Failed to record early exit in ledger: {}", e);
    }

    // Update stats after exit
    let ledger = storage::read_ledger()?;
    let updated_stats = stats::compute(&ledger);
    storage::write_stats(&updated_stats)?;

    position_mgr.clear_position();
    Ok(())
}

async fn fetch_btc_price(price_feed: &dyn PriceFeed) -> Option<PriceSnapshot> {
    let symbol = "BTCUSDT";

    let (candles_1m, candles_5m, spot) = tokio::join!(
        price_feed.candles(symbol, "1m", 15),
        price_feed.candles(symbol, "5m", 12),
        price_feed.spot_price(symbol),
    );

    let candles_1m = candles_1m.ok().flatten()?;
    let candles_5m = candles_5m.ok().flatten()?;
    let spot = spot.ok().flatten()?;

    if candles_1m.is_empty() {
        tracing::warn!("Binance returned empty 1m candles");
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
