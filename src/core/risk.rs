use crate::core::types::{Config, Stats};

pub fn check(
    stats: &Stats,
    balance_cents: u64,
    config: &Config,
) -> Option<String> {
    if balance_cents < config.min_balance_cents {
        return Some(format!(
            "Balance {}¢ < {}¢ minimum",
            balance_cents, config.min_balance_cents
        ));
    }
    if stats.today_pnl_cents <= -config.max_daily_loss_cents {
        return Some(format!("Daily loss: {}¢", stats.today_pnl_cents));
    }
    if stats.current_streak <= -(config.max_consecutive_losses as i32) {
        return Some(format!(
            "{}× consecutive losses",
            stats.current_streak.abs()
        ));
    }
    None
}

/// Half-Kelly position sizing.
/// Returns number of shares (1..=max_shares), or 0 if Kelly says no bet.
pub fn kelly_shares(win_prob: f64, price_cents: u32, max_shares: u32) -> u32 {
    if win_prob <= 0.0 || win_prob >= 1.0 || price_cents == 0 || price_cents >= 100 {
        return 0;
    }

    let b = (100.0 - price_cents as f64) / price_cents as f64; // payout ratio
    let f = (win_prob * b - (1.0 - win_prob)) / b; // Kelly fraction

    if f <= 0.0 {
        return 0;
    }

    let half_kelly = f * 0.5;
    // Scale fraction to shares: fraction * 5, ceil, capped at max_shares and 3
    let shares = (half_kelly * 5.0).ceil() as u32;
    shares.clamp(1, max_shares.min(3))
}

/// Validate that a trade has sufficient edge. Returns None if OK, or a veto reason.
pub fn validate_edge(
    estimated_probability: Option<f64>,
    estimated_edge: Option<f64>,
    price_cents: u32,
    current_streak: i32,
) -> Option<String> {
    // Must provide a probability estimate
    let prob = match estimated_probability {
        Some(p) if (1.0..=99.0).contains(&p) => p,
        Some(p) => return Some(format!("Probability {:.0} out of valid range [1,99]", p)),
        None => return Some("No estimated_probability provided — blocking trade".into()),
    };

    let edge = match estimated_edge {
        Some(e) => e,
        None => {
            // Compute edge from probability and price
            let implied = price_cents as f64;
            (prob - implied).abs()
        }
    };

    // Losing streak protocol: -3 or worse requires 12+ point edge
    let min_edge = if current_streak <= -3 { 12.0 } else { 8.0 };

    if edge < min_edge {
        return Some(format!(
            "Edge {:.1}pt < {:.0}pt minimum (streak={}, prob={:.0}, price={}¢)",
            edge, min_edge, current_streak, prob, price_cents
        ));
    }

    // Price discipline: never pay more than 50¢
    if price_cents > 50 {
        return Some(format!("Price {}¢ > 50¢ max", price_cents));
    }

    None
}
