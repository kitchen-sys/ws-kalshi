use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct MarketsResponse {
    #[serde(default)]
    pub markets: Vec<KalshiMarket>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct KalshiMarket {
    pub ticker: String,
    #[serde(default)]
    pub event_ticker: String,
    pub market_type: Option<String>,
    #[serde(default)]
    pub title: String,
    pub subtitle: Option<String>,
    pub open_time: Option<String>,
    pub close_time: Option<String>,
    pub expiration_time: Option<String>,
    pub expected_expiration_time: Option<String>,
    pub status: Option<String>,
    pub yes_bid: Option<u32>,
    pub yes_ask: Option<u32>,
    pub no_bid: Option<u32>,
    pub no_ask: Option<u32>,
    pub last_price: Option<u32>,
    pub volume: Option<u64>,
    pub volume_24h: Option<u64>,
    pub open_interest: Option<u64>,
    pub result: Option<String>,
    pub series_ticker: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OrderbookResponse {
    pub orderbook: OrderbookData,
}

#[derive(Debug, Deserialize)]
pub struct OrderbookData {
    pub yes: Option<Vec<Vec<u64>>>,
    pub no: Option<Vec<Vec<u64>>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateOrderResponse {
    pub order: OrderInfo,
}

#[derive(Debug, Deserialize)]
pub struct OrderInfo {
    pub order_id: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct BalanceResponse {
    pub balance: u64,
}

#[derive(Debug, Deserialize)]
pub struct PositionsResponse {
    #[serde(default)]
    pub market_positions: Vec<KalshiPosition>,
}

#[derive(Debug, Deserialize)]
pub struct KalshiPosition {
    pub ticker: String,
    pub market_exposure: Option<i64>,
    pub resting_orders_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct OrdersResponse {
    #[serde(default)]
    pub orders: Vec<KalshiOrder>,
}

#[derive(Debug, Deserialize)]
pub struct KalshiOrder {
    pub order_id: String,
    pub ticker: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct SettlementsResponse {
    #[serde(default)]
    pub settlements: Vec<KalshiSettlement>,
}

#[derive(Debug, Deserialize)]
pub struct KalshiSettlement {
    pub ticker: String,
    pub market_result: String,
    pub revenue: Option<i64>,
    pub settled_time: Option<String>,
}
