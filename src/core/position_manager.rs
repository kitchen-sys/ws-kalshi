use crate::core::types::*;

pub struct PositionManager {
    position: Option<OpenPosition>,
    latest_orderbook: Option<OrderbookUpdate>,
    tp_cents: u32,
    sl_cents: u32,
}

impl PositionManager {
    pub fn new(config: &Config) -> Self {
        Self {
            position: None,
            latest_orderbook: None,
            tp_cents: config.tp_cents_per_share,
            sl_cents: config.sl_cents_per_share,
        }
    }

    pub fn has_position(&self) -> bool {
        self.position.is_some()
    }

    pub fn position(&self) -> Option<&OpenPosition> {
        self.position.as_ref()
    }

    pub fn on_fill(&mut self, fill: &FillEvent) {
        self.position = Some(OpenPosition {
            ticker: fill.ticker.clone(),
            side: fill.side.clone(),
            shares: fill.shares,
            entry_price_cents: fill.price_cents,
            order_id: fill.order_id.clone(),
            entered_at: chrono::Utc::now().to_rfc3339(),
        });
        tracing::info!(
            "Position opened: {:?} {}x @ {}¢ on {}",
            fill.side, fill.shares, fill.price_cents, fill.ticker
        );
    }

    pub fn on_orderbook_update(&mut self, update: OrderbookUpdate) {
        self.latest_orderbook = Some(update);
    }

    /// Returns the unrealized P&L per share in cents based on the best exit price.
    /// Positive = profit, negative = loss.
    pub fn unrealized_pnl_per_share(&self) -> Option<i32> {
        let pos = self.position.as_ref()?;
        let ob = self.latest_orderbook.as_ref()?;

        let exit_price = best_exit_price(pos, ob)?;
        let entry = pos.entry_price_cents as i32;
        let exit = exit_price as i32;

        // If we bought YES at 30¢, we sell YES. Profit = sell_price - buy_price.
        // If we bought NO at 30¢, we sell NO. Profit = sell_price - buy_price.
        Some(exit - entry)
    }

    /// Check whether TP or SL threshold has been reached.
    pub fn check_exit(&self) -> Option<ExitReason> {
        let pnl = self.unrealized_pnl_per_share()?;

        if pnl >= self.tp_cents as i32 {
            Some(ExitReason::TakeProfit)
        } else if pnl <= -(self.sl_cents as i32) {
            Some(ExitReason::StopLoss)
        } else {
            None
        }
    }

    /// Build an exit order (sell our side at the best available bid).
    pub fn build_exit_order(&self) -> Option<OrderRequest> {
        let pos = self.position.as_ref()?;
        let ob = self.latest_orderbook.as_ref()?;
        let exit_price = best_exit_price(pos, ob)?;

        Some(OrderRequest {
            ticker: pos.ticker.clone(),
            side: pos.side.clone(),
            shares: pos.shares,
            price_cents: exit_price,
        })
    }

    /// Build an ExitEvent for ledger recording.
    pub fn build_exit_event(&self, reason: ExitReason) -> Option<ExitEvent> {
        let pos = self.position.as_ref()?;
        let ob = self.latest_orderbook.as_ref()?;
        let exit_price = best_exit_price(pos, ob)?;
        let pnl_per_share = exit_price as i64 - pos.entry_price_cents as i64;
        let total_pnl = pnl_per_share * pos.shares as i64;

        Some(ExitEvent {
            ticker: pos.ticker.clone(),
            reason,
            entry_price_cents: pos.entry_price_cents,
            exit_price_cents: exit_price,
            shares: pos.shares,
            pnl_cents: total_pnl,
            order_id: pos.order_id.clone(),
        })
    }

    /// Clear position after a successful exit or settlement.
    pub fn clear_position(&mut self) {
        if let Some(pos) = &self.position {
            tracing::info!("Position cleared: {}", pos.ticker);
        }
        self.position = None;
    }
}

/// Get the best exit price for selling our position.
/// To sell YES, we look at the best YES bid.
/// To sell NO, we look at the best NO bid.
fn best_exit_price(pos: &OpenPosition, ob: &OrderbookUpdate) -> Option<u32> {
    let bids = match pos.side {
        Side::Yes => &ob.yes,
        Side::No => &ob.no,
    };
    // Bids are (price, quantity); take the highest price.
    bids.iter().map(|(price, _qty)| *price).max()
}
