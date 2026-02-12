use crate::core::types::*;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Exchange: Send + Sync {
    async fn active_market(&self) -> Result<Option<MarketState>>;
    async fn orderbook(&self, ticker: &str) -> Result<Orderbook>;
    async fn resting_orders(&self) -> Result<Vec<RestingOrder>>;
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult>;
    async fn positions(&self) -> Result<Vec<Position>>;
    async fn settlements(&self, ticker: &str) -> Result<Vec<Settlement>>;
    async fn balance(&self) -> Result<u64>;
}
