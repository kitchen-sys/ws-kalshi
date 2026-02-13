use crate::core::types::{LedgerRow, Settlement, Stats};
use std::io::Write;

pub fn read_prompt() -> anyhow::Result<String> {
    Ok(std::fs::read_to_string("brain/prompt.md")?)
}

pub fn read_ledger() -> anyhow::Result<Vec<LedgerRow>> {
    let path = "brain/ledger.md";
    let backup = "brain/ledger.md.bak";

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            tracing::warn!("ledger.md unreadable — trying backup");
            std::fs::read_to_string(backup)?
        }
    };

    let rows = parse_ledger_content(&content);

    let data_lines = content
        .lines()
        .filter(|l| l.starts_with('|') && !l.contains("---") && !l.contains("Timestamp"))
        .count();

    if data_lines > 0 && rows.is_empty() {
        tracing::error!(
            "ledger.md corrupt ({} lines, 0 parsed) — using backup",
            data_lines
        );
        let backup_content = std::fs::read_to_string(backup)?;
        return Ok(parse_ledger_content(&backup_content));
    }

    Ok(rows)
}

fn parse_ledger_content(content: &str) -> Vec<LedgerRow> {
    content
        .lines()
        .filter(|l| l.starts_with('|') && !l.contains("---") && !l.contains("Timestamp"))
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if cols.len() < 9 {
                return None;
            }
            let order_id = if cols.len() >= 10 {
                cols[9].to_string()
            } else {
                String::new()
            };
            Some(LedgerRow {
                timestamp: cols[1].to_string(),
                ticker: cols[2].to_string(),
                side: cols[3].to_string(),
                shares: cols[4].parse().ok()?,
                price: cols[5].parse().ok()?,
                result: cols[6].to_string(),
                pnl_cents: cols[7].parse().ok()?,
                cumulative_cents: cols[8].parse().ok()?,
                order_id,
            })
        })
        .collect()
}

pub fn append_ledger(row: &LedgerRow) -> anyhow::Result<()> {
    let path = "brain/ledger.md";
    let backup = "brain/ledger.md.bak";

    if std::path::Path::new(path).exists() {
        std::fs::copy(path, backup)?;
    }

    let line = format!(
        "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        row.timestamp,
        row.ticker,
        row.side,
        row.shares,
        row.price,
        row.result,
        row.pnl_cents,
        row.cumulative_cents,
        row.order_id
    );

    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    writeln!(file, "{}", line)?;

    Ok(())
}

pub fn settle_last_trade(settlement: &Settlement) -> anyhow::Result<()> {
    let path = "brain/ledger.md";
    let backup = "brain/ledger.md.bak";

    if std::path::Path::new(path).exists() {
        std::fs::copy(path, backup)?;
    }

    let content = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Find the last pending line and update it
    for line in lines.iter_mut().rev() {
        if line.contains("| pending |") {
            let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if cols.len() >= 9 {
                let shares: i64 = cols[4].parse().unwrap_or(1);
                let price: i64 = cols[5].parse().unwrap_or(0);
                let cost = price * shares;
                let pnl = settlement.pnl_cents - cost;
                let prev_cumulative: i64 = cols[8].parse().unwrap_or(0);
                let new_cumulative = prev_cumulative + pnl;
                let order_id = if cols.len() >= 10 { cols[9] } else { "" };
                *line = format!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    cols[1],
                    cols[2],
                    cols[3],
                    cols[4],
                    cols[5],
                    settlement.result,
                    pnl,
                    new_cumulative,
                    order_id
                );
            }
            break;
        }
    }

    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

pub fn cancel_trade(order_id: &str) -> anyhow::Result<()> {
    let path = "brain/ledger.md";
    let backup = "brain/ledger.md.bak";

    if std::path::Path::new(path).exists() {
        std::fs::copy(path, backup)?;
    }

    let content = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    for line in lines.iter_mut().rev() {
        if line.contains("| pending |") && line.contains(order_id) {
            let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if cols.len() >= 9 {
                let oid = if cols.len() >= 10 { cols[9] } else { "" };
                *line = format!(
                    "| {} | {} | {} | {} | {} | cancelled | 0 | {} | {} |",
                    cols[1], cols[2], cols[3], cols[4], cols[5], cols[8], oid
                );
            }
            break;
        }
    }

    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

pub fn record_early_exit(exit: &crate::core::types::ExitEvent) -> anyhow::Result<()> {
    let path = "brain/ledger.md";
    let backup = "brain/ledger.md.bak";

    if std::path::Path::new(path).exists() {
        std::fs::copy(path, backup)?;
    }

    let content = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Find the last pending line for this ticker and update it
    for line in lines.iter_mut().rev() {
        if line.contains("| pending |") && line.contains(&exit.ticker) {
            let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if cols.len() >= 9 {
                let prev_cumulative: i64 = cols[8].parse().unwrap_or(0);
                let new_cumulative = prev_cumulative + exit.pnl_cents;
                let order_id = if cols.len() >= 10 { cols[9] } else { "" };
                let result_str = format!("exit_{}", exit.reason);
                *line = format!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    cols[1],
                    cols[2],
                    cols[3],
                    cols[4],
                    cols[5],
                    result_str,
                    exit.pnl_cents,
                    new_cumulative,
                    order_id
                );
            }
            break;
        }
    }

    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

pub fn write_stats(stats: &Stats) -> anyhow::Result<()> {
    let content = format!(
        "# Stats\n\
         - Total trades: {}\n\
         - Wins: {} | Losses: {}\n\
         - Win rate: {:.1}%\n\
         - Total P&L: {}¢\n\
         - Today P&L: {}¢\n\
         - Streak: {}\n\
         - Max drawdown: {}¢\n\
         - Avg win: {:.0}¢ | Avg loss: {:.0}¢\n",
        stats.total_trades,
        stats.wins,
        stats.losses,
        stats.win_rate * 100.0,
        stats.total_pnl_cents,
        stats.today_pnl_cents,
        stats.current_streak,
        stats.max_drawdown_cents,
        stats.avg_win_cents,
        stats.avg_loss_cents,
    );

    std::fs::write("brain/stats.md.tmp", &content)?;
    std::fs::rename("brain/stats.md.tmp", "brain/stats.md")?;
    Ok(())
}
