mod adapters;
mod core;
mod ports;
mod safety;
mod storage;

use adapters::binance::BinanceClient;
use adapters::kalshi::client::KalshiClient;
use adapters::openrouter::OpenRouterClient;
use core::types::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenv::dotenv() {
        eprintln!("WARNING: .env load failed: {}", e);
    }
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    tracing::info!("paper_trade={} confirm_live={}", config.paper_trade, config.confirm_live);

    safety::validate_startup(&config)?;

    let _lock = safety::Lockfile::acquire(&config.lockfile_path)?;

    let exchange = KalshiClient::new(&config)?;
    let brain = OpenRouterClient::new(&config)?;
    let price_feed = BinanceClient::new(&config)?;

    core::engine::run_cycle(&exchange, &brain, &price_feed, &config).await
}
