use crate::core::types::*;

/// Standard RSI over `period` candles (typically 9).
pub fn compute_rsi(candles: &[Candle], period: usize) -> f64 {
    if candles.len() < period + 1 {
        return 50.0; // neutral when insufficient data
    }

    let mut gains = 0.0;
    let mut losses = 0.0;
    let start = candles.len().saturating_sub(period + 1);
    let slice = &candles[start..];

    for w in slice.windows(2) {
        let change = w[1].close - w[0].close;
        if change > 0.0 {
            gains += change;
        } else {
            losses += change.abs();
        }
    }

    let avg_gain = gains / period as f64;
    let avg_loss = losses / period as f64;

    if avg_loss == 0.0 {
        return 100.0;
    }

    let rs = avg_gain / avg_loss;
    100.0 - (100.0 / (1.0 + rs))
}

/// Exponential moving average over `period` candles (typically 9).
pub fn compute_ema(candles: &[Candle], period: usize) -> f64 {
    if candles.is_empty() {
        return 0.0;
    }
    if candles.len() <= period {
        return candles.iter().map(|c| c.close).sum::<f64>() / candles.len() as f64;
    }

    let multiplier = 2.0 / (period as f64 + 1.0);

    // Seed with SMA of first `period` candles
    let sma: f64 = candles[..period].iter().map(|c| c.close).sum::<f64>() / period as f64;

    candles[period..].iter().fold(sma, |ema, c| {
        (c.close - ema) * multiplier + ema
    })
}

/// Distance-weighted bid/ask volume ratio.
/// > 1.0 means bid-heavy (buying pressure), < 1.0 means ask-heavy.
pub fn compute_orderbook_imbalance(orderbook: &Orderbook) -> f64 {
    fn weighted_volume(levels: &[(u32, u32)]) -> f64 {
        levels
            .iter()
            .enumerate()
            .take(5)
            .map(|(i, (_price, qty))| {
                let weight = 1.0 / (i as f64 + 1.0);
                *qty as f64 * weight
            })
            .sum()
    }

    let bid_vol = weighted_volume(&orderbook.yes);
    let ask_vol = weighted_volume(&orderbook.no);

    if ask_vol == 0.0 {
        if bid_vol > 0.0 { 5.0 } else { 1.0 }
    } else {
        (bid_vol / ask_vol).clamp(0.2, 5.0)
    }
}

/// Check if 5m, 15m, and 1h trends all agree.
pub fn compute_trend_alignment(pct_5m: f64, pct_15m: f64, pct_1h: f64) -> TrendAlignment {
    let threshold = 0.05;
    let up_5m = pct_5m > threshold;
    let up_15m = pct_15m > threshold;
    let up_1h = pct_1h > threshold;
    let down_5m = pct_5m < -threshold;
    let down_15m = pct_15m < -threshold;
    let down_1h = pct_1h < -threshold;
    let flat_5m = !up_5m && !down_5m;
    let flat_15m = !up_15m && !down_15m;
    let flat_1h = !up_1h && !down_1h;

    if up_5m && up_15m && up_1h {
        TrendAlignment::AllUp
    } else if down_5m && down_15m && down_1h {
        TrendAlignment::AllDown
    } else if flat_5m && flat_15m && flat_1h {
        TrendAlignment::AllFlat
    } else {
        TrendAlignment::Mixed
    }
}

/// Master signal summary function.
/// Builds a probability estimate from all indicators, computes edge, picks side,
/// computes half-Kelly shares, and generates a narrative for the LLM.
pub fn compute_signal_summary(
    indicators: &PriceIndicators,
    orderbook: &Orderbook,
    market: &MarketState,
) -> SignalSummary {
    // Start at 50% base probability for YES
    let mut prob_yes: f64 = 50.0;

    // Momentum adjustment (±0.15% threshold, raised from ±0.05%)
    if indicators.pct_change_15m > 0.15 {
        prob_yes += 8.0;
    } else if indicators.pct_change_15m < -0.15 {
        prob_yes -= 8.0;
    } else if indicators.pct_change_15m > 0.05 {
        prob_yes += 3.0;
    } else if indicators.pct_change_15m < -0.05 {
        prob_yes -= 3.0;
    }

    // Trend alignment bonus
    let trend = compute_trend_alignment(
        indicators.pct_change_5m,
        indicators.pct_change_15m,
        indicators.pct_change_1h,
    );
    match trend {
        TrendAlignment::AllUp => prob_yes += 6.0,
        TrendAlignment::AllDown => prob_yes -= 6.0,
        _ => {}
    }

    // EMA alignment
    let ema_diff_pct = if indicators.ema_9 > 0.0 {
        ((indicators.spot_price - indicators.ema_9) / indicators.ema_9) * 100.0
    } else {
        0.0
    };
    if ema_diff_pct > 0.05 {
        prob_yes += 3.0;
    } else if ema_diff_pct < -0.05 {
        prob_yes -= 3.0;
    }

    // RSI signal
    let rsi = indicators.rsi_9;
    let rsi_signal = if rsi > 70.0 {
        prob_yes += 4.0; // overbought = likely to stay up in 15min
        "OVERBOUGHT (>70)".to_string()
    } else if rsi < 30.0 {
        prob_yes -= 4.0; // oversold = likely to stay down
        "OVERSOLD (<30)".to_string()
    } else {
        "NEUTRAL".to_string()
    };

    // Orderbook imbalance
    let imbalance = compute_orderbook_imbalance(orderbook);
    if imbalance > 2.0 {
        prob_yes += 3.0; // heavy yes-side buying
    } else if imbalance < 0.5 {
        prob_yes -= 3.0; // heavy no-side buying
    }

    // Clamp to [5, 95]
    prob_yes = prob_yes.clamp(5.0, 95.0);

    // Compute edge vs market price for both sides
    let yes_ask = market.yes_ask.unwrap_or(99) as f64;
    let no_ask = market.no_ask.unwrap_or(99) as f64;
    let yes_edge = prob_yes - yes_ask;
    let no_edge = (100.0 - prob_yes) - no_ask;

    // Pick side with larger edge (must be > 0 to recommend)
    let (recommended_side, best_edge, best_price) = if yes_edge > no_edge && yes_edge > 0.0 {
        (Some(Side::Yes), yes_edge, yes_ask)
    } else if no_edge > 0.0 {
        (Some(Side::No), no_edge, no_ask)
    } else {
        (None, yes_edge.max(no_edge), yes_ask.min(no_ask))
    };

    // Half-Kelly shares (delegated to risk module, but compute locally for summary)
    let win_prob = if recommended_side == Some(Side::Yes) {
        prob_yes / 100.0
    } else {
        (100.0 - prob_yes) / 100.0
    };
    let kelly = if best_price > 0.0 && best_price < 100.0 && win_prob > 0.0 {
        let b = (100.0 - best_price) / best_price; // payout ratio
        let f = (win_prob * b - (1.0 - win_prob)) / b;
        (f * 0.5).max(0.0) // half-Kelly fraction
    } else {
        0.0
    };
    // Convert Kelly fraction to shares (max 3)
    let kelly_shares = if best_edge >= 8.0 {
        (kelly * 5.0).ceil().clamp(1.0, 3.0) as u32
    } else {
        0
    };

    // Generate narrative
    let side_label = match &recommended_side {
        Some(Side::Yes) => "YES",
        Some(Side::No) => "NO",
        None => "NONE",
    };
    let narrative = format!(
        "Trend: {} | RSI(9): {:.1} ({}) | EMA(9) gap: {:+.3}% | OB imbalance: {:.2} | \
         Est. prob YES: {:.0}% | Best side: {} edge {:.1}pt | Kelly: {} shares",
        trend, rsi, rsi_signal, ema_diff_pct, imbalance,
        prob_yes, side_label, best_edge, kelly_shares
    );

    SignalSummary {
        trend,
        rsi_signal,
        orderbook_imbalance: imbalance,
        recommended_side,
        estimated_edge: best_edge,
        kelly_shares,
        estimated_probability: prob_yes,
        narrative,
    }
}

pub fn compute(candles_1m: &[Candle], candles_5m: &[Candle], spot: f64) -> PriceIndicators {
    let pct_change_15m = if !candles_1m.is_empty() {
        let first_open = candles_1m.first().unwrap().open;
        ((spot - first_open) / first_open) * 100.0
    } else {
        0.0
    };

    let pct_change_1h = if !candles_5m.is_empty() {
        let first_open = candles_5m.first().unwrap().open;
        ((spot - first_open) / first_open) * 100.0
    } else {
        0.0
    };

    // 5m change from the last candle in 5m series
    let pct_change_5m = if candles_5m.len() >= 1 {
        let last_5m = candles_5m.last().unwrap();
        ((spot - last_5m.open) / last_5m.open) * 100.0
    } else {
        0.0
    };

    // Raised momentum threshold from ±0.05% to ±0.15%
    let momentum = if pct_change_15m > 0.15 {
        MomentumDirection::Up
    } else if pct_change_15m < -0.15 {
        MomentumDirection::Down
    } else {
        MomentumDirection::Flat
    };

    let sma_15m = if !candles_1m.is_empty() {
        candles_1m.iter().map(|c| c.close).sum::<f64>() / candles_1m.len() as f64
    } else {
        spot
    };

    let sma_diff_pct = ((spot - sma_15m) / sma_15m) * 100.0;
    let price_vs_sma = if sma_diff_pct.abs() < 0.01 {
        "at SMA".into()
    } else if sma_diff_pct > 0.0 {
        format!("above +{:.3}%", sma_diff_pct)
    } else {
        format!("below {:.3}%", sma_diff_pct)
    };

    let returns: Vec<f64> = candles_1m
        .windows(2)
        .map(|w| (w[1].close - w[0].close) / w[0].close * 100.0)
        .collect();
    let volatility_1m = if returns.len() >= 2 {
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance =
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
        variance.sqrt()
    } else {
        0.0
    };

    let last_3_candles: Vec<Candle> = candles_1m
        .iter()
        .rev()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    // RSI(9) and EMA(9) from 1m candles
    let rsi_9 = compute_rsi(candles_1m, 9);
    let ema_9 = compute_ema(candles_1m, 9);

    let ema_diff_pct = if ema_9 > 0.0 {
        ((spot - ema_9) / ema_9) * 100.0
    } else {
        0.0
    };
    let price_vs_ema = if ema_diff_pct.abs() < 0.01 {
        "at EMA".into()
    } else if ema_diff_pct > 0.0 {
        format!("above +{:.3}%", ema_diff_pct)
    } else {
        format!("below {:.3}%", ema_diff_pct)
    };

    PriceIndicators {
        spot_price: spot,
        pct_change_15m,
        pct_change_1h,
        pct_change_5m,
        momentum,
        sma_15m,
        price_vs_sma,
        volatility_1m,
        last_3_candles,
        rsi_9,
        ema_9,
        price_vs_ema,
    }
}
