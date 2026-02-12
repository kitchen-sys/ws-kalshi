use crate::core::{indicators, risk, stats, types::*};
use crate::ports::brain::Brain;
use crate::ports::exchange::Exchange;
use crate::ports::price_feed::PriceFeed;
use crate::storage;
use anyhow::Result;

pub async fn run_cycle(
    exchange: &dyn Exchange,
    brain: &dyn Brain,
    price_feed: &dyn PriceFeed,
    config: &Config,
) -> Result<()> {
    // 1. CANCEL stale resting orders from previous cycles
    let resting = exchange.resting_orders().await?;
    for order in &resting {
        exchange.cancel_order(&order.order_id).await?;
        tracing::info!("Canceled stale order: {}", order.order_id);
    }

    // 2. SETTLE — check if previous trade settled, update ledger + stats
    let mut ledger = storage::read_ledger()?;
    if let Some(pending) = ledger.iter().rev().find(|r| r.result == "pending") {
        let settlements = exchange.settlements(&pending.ticker).await?;
        if let Some(s) = settlements.first() {
            storage::settle_last_trade(s)?;
            ledger = storage::read_ledger()?;
            let settled_stats = stats::compute(&ledger);
            storage::write_stats(&settled_stats)?;
            tracing::info!("Settled: {} | {} {}¢", s.result.to_uppercase(), s.ticker, s.pnl_cents);
        }
    }

    // 3. RISK — deterministic checks in Rust
    let computed_stats = stats::compute(&ledger);
    let balance = exchange.balance().await?;
    let positions = exchange.positions().await?;

    if let Some(veto) = risk::check(&computed_stats, balance, &positions, config) {
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

    // 8. FINAL POSITION CHECK — belt and suspenders
    let fresh_positions = exchange.positions().await?;
    if !fresh_positions.is_empty() {
        tracing::warn!("Position appeared during AI call — aborting order");
        return Ok(());
    }

    // 9. EXECUTE — order FIRST, ledger SECOND
    let current_stats = stats::compute(&ledger);

    if config.paper_trade {
        tracing::info!(
            "PAPER: {:?} {}x @ {}¢ | {}",
            side,
            shares,
            price,
            market.ticker
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
            Ok(order_id) => {
                if let Err(e) = storage::append_ledger(&LedgerRow {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    ticker: market.ticker.clone(),
                    side: format!("{:?}", side).to_lowercase(),
                    shares,
                    price,
                    result: "pending".into(),
                    pnl_cents: 0,
                    cumulative_cents: current_stats.total_pnl_cents,
                }) {
                    tracing::error!(
                        "CRITICAL: Order {} placed but ledger write failed: {}",
                        order_id,
                        e
                    );
                    return Err(e.into());
                }

                tracing::info!(
                    "LIVE: {:?} {}x @ {}¢ | {} (order {})",
                    side, shares, price, market.ticker, order_id
                );
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
