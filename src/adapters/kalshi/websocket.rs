use crate::adapters::kalshi::auth::KalshiAuth;
use crate::core::types::*;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite;

#[derive(Debug, Clone)]
pub enum KalshiWsEvent {
    Orderbook(OrderbookUpdate),
    Fill(FillEvent),
    MarketLifecycle(MarketLifecycleEvent),
    Disconnected,
}

pub struct KalshiWsSender {
    cmd_tx: mpsc::Sender<WsCommand>,
}

enum WsCommand {
    Subscribe { channels: Vec<String>, ticker: String },
    Unsubscribe { channels: Vec<String>, ticker: String },
}

impl KalshiWsSender {
    pub async fn subscribe(&self, channels: Vec<String>, ticker: &str) {
        let _ = self.cmd_tx.send(WsCommand::Subscribe {
            channels,
            ticker: ticker.to_string(),
        }).await;
    }

    pub async fn unsubscribe(&self, channels: Vec<String>, ticker: &str) {
        let _ = self.cmd_tx.send(WsCommand::Unsubscribe {
            channels,
            ticker: ticker.to_string(),
        }).await;
    }
}

pub async fn connect(
    ws_url: &str,
    auth: &KalshiAuth,
    event_tx: mpsc::Sender<KalshiWsEvent>,
) -> anyhow::Result<KalshiWsSender> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(32);

    let url = ws_url.to_string();
    let auth_headers = auth.headers("GET", "/trade-api/ws/v2");

    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        ws_loop(&url, auth_headers, event_tx_clone, cmd_rx).await;
    });

    Ok(KalshiWsSender { cmd_tx })
}

async fn ws_loop(
    url: &str,
    auth_headers: Vec<(&'static str, String)>,
    event_tx: mpsc::Sender<KalshiWsEvent>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
) {
    loop {
        tracing::info!("Kalshi WS connecting to {}", url);

        let mut request = match url.parse::<http::Uri>() {
            Ok(uri) => {
                let host = uri.host().unwrap_or("api.elections.kalshi.com");
                match tungstenite::http::Request::builder()
                    .uri(url)
                    .header("Host", host)
                    .header("Connection", "Upgrade")
                    .header("Upgrade", "websocket")
                    .header("Sec-WebSocket-Version", "13")
                    .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
                    .body(())
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("Failed to build WS request: {}", e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                }
            }
            Err(e) => {
                tracing::error!("Invalid WS URL: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        for (k, v) in &auth_headers {
            request.headers_mut().insert(
                http::header::HeaderName::from_static(
                    match *k {
                        "KALSHI-ACCESS-KEY" => "kalshi-access-key",
                        "KALSHI-ACCESS-TIMESTAMP" => "kalshi-access-timestamp",
                        "KALSHI-ACCESS-SIGNATURE" => "kalshi-access-signature",
                        "Content-Type" => "content-type",
                        _ => continue,
                    }
                ),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }

        match connect_async_with_config(request, None, false).await {
            Ok((ws, _)) => {
                tracing::info!("Kalshi WS connected");
                let (mut write, mut read) = ws.split();

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(tungstenite::Message::Text(text))) => {
                                    if let Some(event) = parse_kalshi_message(&text) {
                                        if event_tx.send(event).await.is_err() {
                                            tracing::warn!("Kalshi WS receiver dropped");
                                            return;
                                        }
                                    }
                                }
                                Some(Ok(tungstenite::Message::Close(_))) => {
                                    tracing::warn!("Kalshi WS closed by server");
                                    break;
                                }
                                Some(Err(e)) => {
                                    tracing::warn!("Kalshi WS read error: {}", e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("Kalshi WS stream ended");
                                    break;
                                }
                                _ => {}
                            }
                        }
                        cmd = cmd_rx.recv() => {
                            match cmd {
                                Some(WsCommand::Subscribe { channels, ticker }) => {
                                    let msg = serde_json::json!({
                                        "id": 1,
                                        "cmd": "subscribe",
                                        "params": {
                                            "channels": channels,
                                            "market_tickers": [ticker]
                                        }
                                    });
                                    if let Err(e) = write.send(tungstenite::Message::Text(msg.to_string().into())).await {
                                        tracing::warn!("Kalshi WS send error: {}", e);
                                        break;
                                    }
                                    tracing::info!("Kalshi WS subscribed to {} on {}", channels.join(","), ticker);
                                }
                                Some(WsCommand::Unsubscribe { channels, ticker }) => {
                                    let msg = serde_json::json!({
                                        "id": 2,
                                        "cmd": "unsubscribe",
                                        "params": {
                                            "channels": channels,
                                            "market_tickers": [ticker]
                                        }
                                    });
                                    if let Err(e) = write.send(tungstenite::Message::Text(msg.to_string().into())).await {
                                        tracing::warn!("Kalshi WS send error: {}", e);
                                        break;
                                    }
                                }
                                None => {
                                    tracing::warn!("Kalshi WS command channel closed");
                                    return;
                                }
                            }
                        }
                    }
                }

                let _ = event_tx.send(KalshiWsEvent::Disconnected).await;
            }
            Err(e) => {
                tracing::warn!("Kalshi WS connect failed: {}", e);
            }
        }

        tracing::info!("Kalshi WS reconnecting in 5s");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

fn parse_kalshi_message(text: &str) -> Option<KalshiWsEvent> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let msg_type = v.get("type")?.as_str()?;

    match msg_type {
        "orderbook_snapshot" | "orderbook_delta" => {
            let ticker = v.get("msg")?.get("market_ticker")?.as_str()?.to_string();

            let parse_levels = |key: &str| -> Vec<(u32, u32)> {
                v.get("msg")
                    .and_then(|m| m.get(key))
                    .and_then(|s| s.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|level| {
                                let l = level.as_array()?;
                                if l.len() >= 2 {
                                    Some((l[0].as_u64()? as u32, l[1].as_u64()? as u32))
                                } else {
                                    None
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };

            Some(KalshiWsEvent::Orderbook(OrderbookUpdate {
                ticker,
                yes: parse_levels("yes"),
                no: parse_levels("no"),
            }))
        }
        "fill" => {
            let msg = v.get("msg")?;
            let order_id = msg.get("order_id")?.as_str()?.to_string();
            let ticker = msg.get("market_ticker")?.as_str()?.to_string();
            let side_str = msg.get("side")?.as_str()?;
            let side = match side_str {
                "yes" => Side::Yes,
                "no" => Side::No,
                _ => return None,
            };
            let shares = msg.get("count")?.as_u64()? as u32;
            let price_cents = msg.get("yes_price")
                .and_then(|p| p.as_u64())
                .map(|p| if side == Side::Yes { p as u32 } else { 100 - p as u32 })
                .unwrap_or(0);

            Some(KalshiWsEvent::Fill(FillEvent {
                order_id,
                ticker,
                side,
                shares,
                price_cents,
            }))
        }
        "market_lifecycle" => {
            let msg = v.get("msg")?;
            let ticker = msg.get("market_ticker")?.as_str()?.to_string();
            let status = msg.get("status")?.as_str()?.to_string();
            let result = msg.get("result").and_then(|r| r.as_str()).map(|s| s.to_string());

            Some(KalshiWsEvent::MarketLifecycle(MarketLifecycleEvent {
                ticker,
                status,
                result,
            }))
        }
        _ => None,
    }
}
