use crate::core::types::{Candle, Config};
use crate::ports::price_feed::PriceFeed;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

pub struct BinanceClient {
    client: reqwest::Client,
    base_url: String,
}

impl BinanceClient {
    pub fn new(_config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?,
            base_url: "https://api.binance.us".into(),
        })
    }
}

#[async_trait]
impl PriceFeed for BinanceClient {
    async fn candles(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Option<Vec<Candle>>> {
        let url = format!(
            "{}/api/v3/klines?symbol={}&interval={}&limit={}",
            self.base_url, symbol, interval, limit
        );

        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Binance klines request failed: {}", e);
                return Ok(None);
            }
        };

        if !resp.status().is_success() {
            tracing::warn!("Binance klines -> {}", resp.status());
            return Ok(None);
        }

        let raw: Vec<Vec<serde_json::Value>> = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Binance klines parse error: {}", e);
                return Ok(None);
            }
        };

        let candles = raw
            .into_iter()
            .filter_map(|row| {
                if row.len() < 7 {
                    return None;
                }
                Some(Candle {
                    open_time: row[0].as_i64()?,
                    open: row[1].as_str()?.parse().ok()?,
                    high: row[2].as_str()?.parse().ok()?,
                    low: row[3].as_str()?.parse().ok()?,
                    close: row[4].as_str()?.parse().ok()?,
                    volume: row[5].as_str()?.parse().ok()?,
                    close_time: row[6].as_i64()?,
                })
            })
            .collect();

        Ok(Some(candles))
    }

    async fn spot_price(&self, symbol: &str) -> Result<Option<f64>> {
        let url = format!(
            "{}/api/v3/ticker/price?symbol={}",
            self.base_url, symbol
        );

        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Binance ticker request failed: {}", e);
                return Ok(None);
            }
        };

        if !resp.status().is_success() {
            tracing::warn!("Binance ticker -> {}", resp.status());
            return Ok(None);
        }

        #[derive(Deserialize)]
        struct TickerPrice {
            price: String,
        }

        let ticker: TickerPrice = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Binance ticker parse error: {}", e);
                return Ok(None);
            }
        };

        Ok(ticker.price.parse().ok())
    }
}
