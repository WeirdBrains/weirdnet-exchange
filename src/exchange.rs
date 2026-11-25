//! The adapter traits.
//!
//! An adapter for a venue implements [`MarketData`] (public data, no credentials) and, with
//! credentials, [`Trading`]. Anything that implements both is an [`Exchange`]. Apps hold
//! `Arc<dyn Exchange>` (or the narrower trait) and don't care which venue is behind it;
//! [`crate::Paper`] is the in-memory one for tests and dry runs.
//!
//! Placing is idempotent per [`ClientOrderId`]: placing the same request again returns the
//! first acknowledgement instead of a second order, so a placement whose outcome is unknown
//! (a timeout) can be retried. Adapters for venues without that guarantee look the client id
//! up before resending.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::account::{Balance, FeeTier, Fill, MarginSummary, Position};
use crate::instrument::{Product, ProductId, VenueId};
use crate::market::{BookSnapshot, Candle, Channel, Granularity, MarketEvent, Ticker, Trade};
use crate::num::{Decimal, Price, Qty};
use crate::order::{ClientOrderId, Order, OrderAck, OrderId, OrderRequest, OrderType, TimeInForce};
use crate::{ExchangeError, RejectReason, Result};

/// Test (sandbox, paper) or live (real money).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Test,
    Live,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Test => "test",
            Environment::Live => "live",
        }
    }
}

/// A stream of events. An `Err` item reports a problem (such as a lagging consumer that missed
/// events); the stream may continue after it. The stream ends when the connection does.
pub type EventStream<T> = BoxStream<'static, Result<T>>;

/// What an adapter supports. Unset fields are unsupported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Capabilities {
    pub market_orders: bool,
    pub limit_orders: bool,
    pub stop_orders: bool,
    pub post_only: bool,
    pub ioc: bool,
    pub fok: bool,
    pub gtd: bool,
    /// Market orders sized in the quote asset.
    pub quote_size: bool,
    pub reduce_only: bool,
    pub preview: bool,
    pub cancel_all: bool,
    pub positions: bool,
    pub margin: bool,
    pub user_stream: bool,
}

impl Capabilities {
    /// Everything on.
    pub fn all() -> Self {
        Self {
            market_orders: true,
            limit_orders: true,
            stop_orders: true,
            post_only: true,
            ioc: true,
            fok: true,
            gtd: true,
            quote_size: true,
            reduce_only: true,
            preview: true,
            cancel_all: true,
            positions: true,
            margin: true,
            user_stream: true,
        }
    }

    /// Checks a request uses only supported features.
    pub fn check(&self, r: &OrderRequest) -> Result<()> {
        let ok = match r.order_type {
            OrderType::Market => self.market_orders,
            OrderType::Limit { post_only, .. } => self.limit_orders && (!post_only || self.post_only),
            OrderType::StopLimit { .. } | OrderType::StopMarket { .. } => self.stop_orders,
        } && match r.time_in_force {
            TimeInForce::Gtc => true,
            TimeInForce::Gtd { .. } => self.gtd,
            TimeInForce::Ioc => self.ioc || matches!(r.order_type, OrderType::Market),
            TimeInForce::Fok => self.fok,
        } && (r.base_size().is_some() || self.quote_size)
            && (!r.reduce_only || self.reduce_only);
        if ok { Ok(()) } else { Err(ExchangeError::rejected(RejectReason::UnsupportedOrder, "order type, time in force or size not supported")) }
    }
}

/// A page of results; pass `next` back as the query's cursor for more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

/// Candles for one product over `[start_ms, end_ms)`, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandleQuery {
    pub product: ProductId,
    pub granularity: Granularity,
    pub start_ms: i64,
    pub end_ms: i64,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// Which orders to list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderQuery {
    pub product: Option<ProductId>,
    /// Only orders that may still fill.
    pub open_only: bool,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// Which fills to list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillQuery {
    pub product: Option<ProductId>,
    pub order_id: Option<OrderId>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// An order by the venue's id or the client's.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderRef {
    Id(OrderId),
    Client(ClientOrderId),
}

impl From<OrderId> for OrderRef {
    fn from(id: OrderId) -> Self {
        OrderRef::Id(id)
    }
}

impl From<ClientOrderId> for OrderRef {
    fn from(id: ClientOrderId) -> Self {
        OrderRef::Client(id)
    }
}

/// What an order would do if placed now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    /// Base size that would fill immediately.
    pub size: Qty,
    pub notional: Decimal,
    pub average_price: Option<Price>,
    pub fee: Decimal,
    pub warnings: Vec<String>,
}

/// Something that happened on the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UserEvent {
    /// An order changed; the latest state.
    Order(Order),
    Fill(Fill),
    Balance(Balance),
    Position(Position),
}

/// Public market data.
#[async_trait]
pub trait MarketData: Send + Sync {
    fn venue(&self) -> &VenueId;

    async fn products(&self) -> Result<Vec<Product>>;

    async fn product(&self, id: &ProductId) -> Result<Product> {
        self.products().await?.into_iter().find(|p| p.market.product == *id).ok_or_else(|| ExchangeError::not_found(format!("product {id}")))
    }

    async fn ticker(&self, product: &ProductId) -> Result<Ticker>;

    /// The book, `depth` levels a side (all for `None`).
    async fn book(&self, product: &ProductId, depth: Option<usize>) -> Result<BookSnapshot>;

    /// Recent public trades, newest first.
    async fn trades(&self, product: &ProductId, limit: usize) -> Result<Vec<Trade>> {
        let _ = (product, limit);
        Err(ExchangeError::unsupported("recent trades"))
    }

    async fn candles(&self, query: &CandleQuery) -> Result<Page<Candle>>;

    /// The venue's clock, in milliseconds since the epoch.
    async fn server_time(&self) -> Result<i64> {
        Ok(crate::now_ms())
    }

    /// Live events for these products and channels. A [`Channel::Book`] subscription starts
    /// with a [`MarketEvent::Book`] snapshot, then deltas.
    async fn subscribe(&self, products: &[ProductId], channels: &[Channel]) -> Result<EventStream<MarketEvent>>;
}

/// Trading on an account.
#[async_trait]
pub trait Trading: Send + Sync {
    /// Test or live; an adapter built from [`crate::Credential<Live>`](crate::Credential) is live.
    fn environment(&self) -> Environment;

    fn capabilities(&self) -> Capabilities;

    async fn balances(&self) -> Result<Vec<Balance>>;

    async fn fee_tier(&self) -> Result<FeeTier>;

    async fn preview_order(&self, request: &OrderRequest) -> Result<Preview> {
        let _ = request;
        Err(ExchangeError::unsupported("order preview"))
    }

    /// Places an order. Idempotent per client id (see the module docs).
    async fn place_order(&self, request: &OrderRequest) -> Result<OrderAck>;

    async fn cancel(&self, order: &OrderRef) -> Result<()>;

    /// Cancels every working order, or those for one product. Returns the ids cancelled.
    async fn cancel_all(&self, product: Option<&ProductId>) -> Result<Vec<OrderId>>;

    async fn order(&self, order: &OrderRef) -> Result<Order>;

    async fn orders(&self, query: &OrderQuery) -> Result<Page<Order>>;

    async fn fills(&self, query: &FillQuery) -> Result<Page<Fill>>;

    async fn positions(&self) -> Result<Vec<Position>> {
        Err(ExchangeError::unsupported("positions"))
    }

    async fn margin(&self) -> Result<MarginSummary> {
        Err(ExchangeError::unsupported("margin"))
    }

    /// Order, fill and balance changes on the account.
    async fn subscribe_user(&self) -> Result<EventStream<UserEvent>>;
}

/// Market data and trading on one venue.
pub trait Exchange: MarketData + Trading {}

impl<T: MarketData + Trading + ?Sized> Exchange for T {}
