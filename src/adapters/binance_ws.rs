use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

#[derive(Debug, Clone)]
pub struct CryptoPriceUpdate {
    pub symbol: String,
    pub price: f64,
}

pub async fn connect(
    url: &str,
    tx: mpsc::Sender<CryptoPriceUpdate>,
) -> anyhow::Result<()> {
    loop {
        tracing::info!("Binance WS connecting to {}", url);
        match connect_async(url).await {
            Ok((ws, _)) => {
                tracing::info!("Binance WS connected");
                let (_, mut read) = ws.split();

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Some(update) = parse_kline(&text) {
                                if tx.send(update).await.is_err() {
                                    tracing::warn!("Binance WS receiver dropped");
                                    return Ok(());
                                }
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                            tracing::warn!("Binance WS closed by server");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("Binance WS error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Binance WS connect failed: {}", e);
            }
        }
        tracing::info!("Binance WS reconnecting in 5s");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

fn parse_kline(text: &str) -> Option<CryptoPriceUpdate> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    // Combined stream format: {"stream":"btcusdt@kline_1m","data":{...}}
    // Single stream format: {"e":"kline","k":{...}}
    let k = if let Some(data) = v.get("data") {
        data.get("k")?
    } else {
        v.get("k")?
    };

    let close_str = k.get("c")?.as_str()?;
    let price = close_str.parse::<f64>().ok()?;
    let symbol = k.get("s")?.as_str()?.to_string();
    Some(CryptoPriceUpdate { symbol, price })
}
