use super::auth::KalshiAuth;
use super::types::*;
use crate::core::types::*;
use crate::ports::exchange::Exchange;
use anyhow::Result;
use async_trait::async_trait;
use serde::de::DeserializeOwned;

pub struct KalshiClient {
    client: reqwest::Client,
    auth: KalshiAuth,
    base_url: String,
    series_ticker: String,
}

impl KalshiClient {
    pub fn new(config: &Config) -> Result<Self> {
        let auth = KalshiAuth::new(
            config.kalshi_key_id.clone(),
            &config.kalshi_private_key_pem,
        )?;
        Ok(Self {
            client: reqwest::Client::new(),
            auth,
            base_url: config.kalshi_base_url.clone(),
            series_ticker: config.series_ticker.clone(),
        })
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        let mut attempts = 0;
        loop {
            let headers = self.auth.headers(method.as_str(), path);
            let url = format!("{}{}", self.base_url, path);

            let mut req = self.client.request(method.clone(), &url);
            for (k, v) in &headers {
                req = req.header(*k, v);
            }
            if let Some(b) = body {
                req = req.json(b);
            }

            let resp = req.send().await?;
            let status = resp.status();

            if status == 429 && attempts < 1 {
                attempts += 1;
                tracing::warn!("Kalshi 429 — retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }

            if !status.is_success() {
                let err_body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Kalshi {} {} -> {} : {}", method, path, status, err_body);
            }

            let text = resp.text().await?;
            return serde_json::from_str::<T>(&text).map_err(|e| {
                tracing::error!("Deserialize error on {}: {} (body: {}...)", path, e, &text[..text.len().min(300)]);
                e.into()
            });
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.request(reqwest::Method::GET, path, None).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &serde_json::Value) -> Result<T> {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    async fn delete_request(&self, path: &str) -> Result<()> {
        let headers = self.auth.headers("DELETE", path);
        let url = format!("{}{}", self.base_url, path);

        let mut req = self.client.delete(&url);
        for (k, v) in &headers {
            req = req.header(*k, v);
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Kalshi DELETE {} -> {}", path, err_body);
        }
        Ok(())
    }
}

#[async_trait]
impl Exchange for KalshiClient {
    async fn active_market(&self) -> Result<Option<MarketState>> {
        let path = format!(
            "/trade-api/v2/markets?series_ticker={}&status=open",
            self.series_ticker
        );
        let resp: MarketsResponse = self.get(&path).await?;

        let now = chrono::Utc::now();
        let mut candidates: Vec<_> = resp
            .markets
            .into_iter()
            .filter_map(|m| {
                let exp_str = m.expected_expiration_time.as_deref()
                    .or(m.expiration_time.as_deref())?;
                let exp =
                    chrono::DateTime::parse_from_rfc3339(exp_str)
                        .ok()?
                        .with_timezone(&chrono::Utc);
                let mins = (exp - now).num_seconds() as f64 / 60.0;
                Some((m, mins))
            })
            .filter(|(_, mins)| *mins > 0.0)
            .collect();

        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        Ok(candidates.into_iter().next().map(|(m, mins)| MarketState {
            ticker: m.ticker,
            event_ticker: m.event_ticker,
            title: m.title,
            yes_bid: m.yes_bid,
            yes_ask: m.yes_ask,
            no_bid: m.no_bid,
            no_ask: m.no_ask,
            last_price: m.last_price,
            volume: m.volume.unwrap_or(0),
            volume_24h: m.volume_24h.unwrap_or(0),
            open_interest: m.open_interest.unwrap_or(0),
            expiration_time: m.expected_expiration_time.or(m.expiration_time).unwrap_or_default(),
            minutes_to_expiry: mins,
        }))
    }

    async fn orderbook(&self, ticker: &str) -> Result<Orderbook> {
        let path = format!("/trade-api/v2/markets/{}/orderbook", ticker);
        let resp: OrderbookResponse = self.get(&path).await?;

        let parse_side = |levels: Vec<Vec<u64>>| -> Vec<(u32, u32)> {
            levels
                .into_iter()
                .filter_map(|l| {
                    if l.len() >= 2 {
                        Some((l[0] as u32, l[1] as u32))
                    } else {
                        None
                    }
                })
                .collect()
        };

        Ok(Orderbook {
            yes: parse_side(resp.orderbook.yes.unwrap_or_default()),
            no: parse_side(resp.orderbook.no.unwrap_or_default()),
        })
    }

    async fn resting_orders(&self) -> Result<Vec<RestingOrder>> {
        let path = "/trade-api/v2/portfolio/orders?status=resting";
        let resp: OrdersResponse = self.get(path).await?;

        Ok(resp
            .orders
            .into_iter()
            .map(|o| RestingOrder {
                order_id: o.order_id,
                ticker: o.ticker,
            })
            .collect())
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let path = format!("/trade-api/v2/portfolio/orders/{}", order_id);
        self.delete_request(&path).await
    }

    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        let path = "/trade-api/v2/portfolio/orders";
        let side_str = match order.side {
            Side::Yes => "yes",
            Side::No => "no",
        };
        let body = serde_json::json!({
            "ticker": order.ticker,
            "action": "buy",
            "side": side_str,
            "count": order.shares,
            "type": "limit",
            "yes_price": if order.side == Side::Yes { order.price_cents } else { 100 - order.price_cents },
            "client_order_id": uuid::Uuid::new_v4().to_string(),
        });

        let resp: CreateOrderResponse = self.post(path, &body).await?;
        Ok(OrderResult {
            order_id: resp.order.order_id,
            status: resp.order.status,
        })
    }

    async fn positions(&self) -> Result<Vec<Position>> {
        let path = "/trade-api/v2/portfolio/positions";
        let resp: PositionsResponse = self.get(path).await?;

        Ok(resp
            .market_positions
            .into_iter()
            .filter(|p| p.market_exposure.unwrap_or(0) != 0)
            .map(|p| {
                let exposure = p.market_exposure.unwrap_or(0);
                Position {
                    ticker: p.ticker,
                    side: if exposure > 0 { Side::Yes } else { Side::No },
                    count: exposure.unsigned_abs() as u32,
                }
            })
            .collect())
    }

    async fn settlements(&self, ticker: &str) -> Result<Vec<Settlement>> {
        let path = format!("/trade-api/v2/portfolio/settlements?ticker={}", ticker);
        let resp: SettlementsResponse = self.get(&path).await?;

        Ok(resp
            .settlements
            .into_iter()
            .map(|s| {
                let pnl = s.revenue.unwrap_or(0);
                Settlement {
                    ticker: s.ticker,
                    side: Side::Yes,
                    count: 0,
                    price_cents: 0,
                    result: if pnl > 0 {
                        "win".into()
                    } else {
                        "loss".into()
                    },
                    pnl_cents: pnl,
                    settled_time: s.settled_time.unwrap_or_default(),
                    market_result: s.market_result,
                }
            })
            .collect())
    }

    async fn balance(&self) -> Result<u64> {
        let path = "/trade-api/v2/portfolio/balance";
        let resp: BalanceResponse = self.get(path).await?;
        Ok(resp.balance)
    }
}
