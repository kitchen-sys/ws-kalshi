use crate::core::types::*;
use std::collections::HashMap;

pub struct PositionManager {
    /// Open positions keyed by market ticker (e.g., "KXBTC15M-26FEB122045-45")
    positions: HashMap<String, OpenPosition>,
    /// Latest orderbook per market ticker
    orderbooks: HashMap<String, OrderbookUpdate>,
    tp_cents: u32,
    sl_cents: u32,
}

impl PositionManager {
    pub fn new(config: &Config) -> Self {
        Self {
            positions: HashMap::new(),
            orderbooks: HashMap::new(),
            tp_cents: config.tp_cents_per_share,
            sl_cents: config.sl_cents_per_share,
        }
    }

    pub fn position_count(&self) -> usize {
        self.positions.len()
    }

    /// Check if we hold any position whose market ticker starts with the given series.
    pub fn has_position_for_series(&self, series: &str) -> bool {
        self.positions.keys().any(|t| t.starts_with(series))
    }

    /// Get position for a specific market ticker.
    pub fn position_for_ticker(&self, ticker: &str) -> Option<&OpenPosition> {
        self.positions.get(ticker)
    }

    /// Iterator over all open positions.
    pub fn all_positions(&self) -> impl Iterator<Item = (&String, &OpenPosition)> {
        self.positions.iter()
    }

    /// All market tickers with open positions.
    pub fn position_tickers(&self) -> Vec<String> {
        self.positions.keys().cloned().collect()
    }

    pub fn on_fill(&mut self, fill: &FillEvent) {
        let pos = OpenPosition {
            ticker: fill.ticker.clone(),
            side: fill.side.clone(),
            shares: fill.shares,
            entry_price_cents: fill.price_cents,
            order_id: fill.order_id.clone(),
            entered_at: chrono::Utc::now().to_rfc3339(),
        };
        tracing::info!(
            "Position opened: {:?} {}x @ {}¢ on {} [{} total positions]",
            fill.side, fill.shares, fill.price_cents, fill.ticker,
            self.positions.len() + 1
        );
        self.positions.insert(fill.ticker.clone(), pos);
    }

    pub fn on_orderbook_update(&mut self, update: OrderbookUpdate) {
        self.orderbooks.insert(update.ticker.clone(), update);
    }

    /// Returns the unrealized P&L per share for a specific position.
    pub fn unrealized_pnl_per_share(&self, ticker: &str) -> Option<i32> {
        let pos = self.positions.get(ticker)?;
        let ob = self.orderbooks.get(ticker)?;
        let exit_price = best_exit_price(pos, ob)?;
        Some(exit_price as i32 - pos.entry_price_cents as i32)
    }

    /// Check all positions for TP/SL exits. Returns list of (ticker, reason).
    pub fn check_exits(&self) -> Vec<(String, ExitReason)> {
        let mut exits = Vec::new();
        for (ticker, _pos) in &self.positions {
            if let Some(pnl) = self.unrealized_pnl_per_share(ticker) {
                if pnl >= self.tp_cents as i32 {
                    exits.push((ticker.clone(), ExitReason::TakeProfit));
                } else if pnl <= -(self.sl_cents as i32) {
                    exits.push((ticker.clone(), ExitReason::StopLoss));
                }
            }
        }
        exits
    }

    /// Build an exit order for a specific position.
    pub fn build_exit_order(&self, ticker: &str) -> Option<OrderRequest> {
        let pos = self.positions.get(ticker)?;
        let ob = self.orderbooks.get(ticker)?;
        let exit_price = best_exit_price(pos, ob)?;

        Some(OrderRequest {
            ticker: pos.ticker.clone(),
            side: pos.side.clone(),
            shares: pos.shares,
            price_cents: exit_price,
        })
    }

    /// Build an ExitEvent for ledger recording.
    pub fn build_exit_event(&self, ticker: &str, reason: ExitReason) -> Option<ExitEvent> {
        let pos = self.positions.get(ticker)?;
        let ob = self.orderbooks.get(ticker)?;
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

    /// Clear a specific position after exit or settlement.
    pub fn clear_position(&mut self, ticker: &str) {
        if self.positions.remove(ticker).is_some() {
            tracing::info!("Position cleared: {} [{} remaining]", ticker, self.positions.len());
        }
        self.orderbooks.remove(ticker);
    }
}

fn best_exit_price(pos: &OpenPosition, ob: &OrderbookUpdate) -> Option<u32> {
    let bids = match pos.side {
        Side::Yes => &ob.yes,
        Side::No => &ob.no,
    };
    bids.iter().map(|(price, _qty)| *price).max()
}
