use crate::core::types::*;
use crate::ports::brain::Brain;
use anyhow::Result;
use async_trait::async_trait;

pub struct OpenRouterClient {
    client: reqwest::Client,
    api_key: String,
}

impl OpenRouterClient {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            api_key: config.openrouter_api_key.clone(),
        })
    }
}

#[async_trait]
impl Brain for OpenRouterClient {
    async fn decide(&self, ctx: &DecisionContext) -> Result<TradeDecision> {
        let price_section = match &ctx.crypto_price {
            Some(snap) => format!(
                "\n\n---\n## {} PRICE\n{}",
                ctx.crypto_label,
                format_crypto_price(snap)
            ),
            None => format!("\n\n---\n## {} PRICE\nUnavailable this cycle.", ctx.crypto_label),
        };

        let prompt = format!(
            "{prompt}\n\n---\n## STATS\n{stats}\n\n---\n## LAST {n} TRADES\n{ledger}\n\n---\n## MARKET\n{market}\n\n---\n## ORDERBOOK\nYes bids: {yes_ob}\nNo bids: {no_ob}{price}",
            prompt = ctx.prompt_md,
            stats = format_stats(&ctx.stats),
            n = ctx.last_n_trades.len(),
            ledger = format_ledger(&ctx.last_n_trades),
            market = format_market(&ctx.market),
            yes_ob = format_ob_side(&ctx.orderbook.yes),
            no_ob = format_ob_side(&ctx.orderbook.no),
            price = price_section,
        );

        let body = serde_json::json!({
            "model": "anthropic/claude-opus-4-6",
            "max_tokens": 1200,
            "temperature": 0.2,
            "messages": [{"role": "user", "content": prompt}]
        });

        let resp = self
            .client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://kyzlolabs.com")
            .header("X-Title", "Kalshi BTC Bot")
            .json(&body)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No content in OpenRouter response"))?;

        parse_decision(content)
    }
}

fn format_stats(s: &Stats) -> String {
    format!(
        "Trades: {} | W/L: {}/{} | Win rate: {:.1}% | P&L: {}¢ | Today: {}¢ | Streak: {} | Drawdown: {}¢",
        s.total_trades, s.wins, s.losses, s.win_rate * 100.0,
        s.total_pnl_cents, s.today_pnl_cents, s.current_streak, s.max_drawdown_cents
    )
}

fn format_ledger(trades: &[LedgerRow]) -> String {
    if trades.is_empty() {
        return "No trades yet.".into();
    }
    trades
        .iter()
        .map(|t| {
            format!(
                "{} | {} | {} | {}x @ {}¢ | {} | {}¢",
                t.timestamp, t.ticker, t.side, t.shares, t.price, t.result, t.pnl_cents
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_market(m: &MarketState) -> String {
    format!(
        "Ticker: {} | Title: {} | Yes bid/ask: {:?}/{:?} | No bid/ask: {:?}/{:?} | Last: {:?} | Vol: {} | 24h Vol: {} | OI: {} | Expiry: {} ({:.1}min)",
        m.ticker, m.title, m.yes_bid, m.yes_ask, m.no_bid, m.no_ask,
        m.last_price, m.volume, m.volume_24h, m.open_interest,
        m.expiration_time, m.minutes_to_expiry
    )
}

fn format_ob_side(levels: &[(u32, u32)]) -> String {
    if levels.is_empty() {
        return "empty".into();
    }
    levels
        .iter()
        .take(5)
        .map(|(p, q)| format!("{}¢ x{}", p, q))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_crypto_price(snap: &PriceSnapshot) -> String {
    let ind = &snap.indicators;
    let momentum_str = match ind.momentum {
        MomentumDirection::Up => "UP",
        MomentumDirection::Down => "DOWN",
        MomentumDirection::Flat => "FLAT",
    };

    let mut s = format!(
        "Spot: ${:.2} | 15m change: {:+.3}% | 1h change: {:+.3}% | Momentum: {}\n\
         SMA(15x1m): ${:.2} | Price vs SMA: {} | 1m volatility: {:.4}%",
        ind.spot_price,
        ind.pct_change_15m,
        ind.pct_change_1h,
        momentum_str,
        ind.sma_15m,
        ind.price_vs_sma,
        ind.volatility_1m,
    );

    if !ind.last_3_candles.is_empty() {
        s.push_str("\nLast 3 candles (1m): ");
        let candle_strs: Vec<String> = ind
            .last_3_candles
            .iter()
            .map(|c| {
                format!(
                    "O:{:.0} H:{:.0} L:{:.0} C:{:.0} V:{:.1}",
                    c.open, c.high, c.low, c.close, c.volume
                )
            })
            .collect();
        s.push_str(&candle_strs.join(" | "));
    }

    s
}

fn parse_decision(raw: &str) -> Result<TradeDecision> {
    let json_str = if let Some(s) = raw.find("```json") {
        let start = s + 7;
        let end = raw[start..]
            .find("```")
            .map(|i| start + i)
            .unwrap_or(raw.len());
        &raw[start..end]
    } else if raw.trim().starts_with('{') {
        raw.trim()
    } else if let (Some(s), Some(e)) = (raw.find('{'), raw.rfind('}')) {
        &raw[s..=e]
    } else {
        return Ok(TradeDecision {
            action: Action::Pass,
            side: None,
            shares: None,
            max_price_cents: None,
            reasoning: "Failed to parse AI response".into(),
        });
    };

    serde_json::from_str(json_str.trim()).map_err(Into::into)
}
