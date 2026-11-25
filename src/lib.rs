//! Exchange-neutral trading types and traits for Weirdnet apps.
//!
//! - [`num`]: exact decimal [`Price`] and [`Qty`] (no floats), with increment rounding.
//! - [`instrument`]: venues, products, markets and instrument kinds (spot, dated futures,
//!   perpetuals), with order checks against a product's increments and limits.
//! - [`order`]: sides, order types, time in force, client order ids, requests, and an
//!   [`Order`] state machine that accounts fills.
//! - [`account`]: fills, balances, fee tiers, positions (with a [`PositionBook`]) and margin.
//! - [`market`]: trades, tickers, order book snapshots and deltas (an [`OrderBook`] that
//!   applies them with gap checks), candles and a [`CandleBuilder`].
//! - [`exchange`]: the async adapter traits, [`MarketData`] (no credentials) and [`Trading`],
//!   with [`Capabilities`] and paging; [`Exchange`] is both.
//! - [`cost`]: a [`CostModel`] for fees, spread and slippage from a live book.
//! - [`rate`]: a shared token-bucket [`RateLimiter`].
//! - [`credentials`]: secrets for adapters, typed by environment so a live adapter can only be
//!   built from [`Live`] credentials.
//! - [`paper`]: [`Paper`], an in-memory simulated exchange for tests and dry runs.
//! - Feature `vault`: [`vault::VaultSecret`], credentials from `weirdnet-vault`.
//! - Feature `service`: [`feeds::MarketHubs`], market data as `weirdnet-service` hub topics.
//!
//! This crate has no exchange adapters and makes no network calls. Adapters implement the
//! traits in their own crates.
//!
//! Timestamps are milliseconds since the Unix epoch (UTC). Amounts are [`Decimal`].
//!
//! ```
//! use weirdnet_exchange::*;
//!
//! # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
//! let paper = Paper::default();
//! let m = paper.market("BTC-USD");
//! paper.add_product(Product::spot(m.clone(), "BTC", "USD").with_increments(Decimal::new(1, 2), Decimal::new(1, 4)))?;
//! paper.deposit("USD", Decimal::from(1000));
//! paper.set_book(BookSnapshot {
//!     market: m.clone(),
//!     bids: vec![Level::new(Price::parse("99")?, Qty::parse("1")?)],
//!     asks: vec![Level::new(Price::parse("101")?, Qty::parse("1")?)],
//!     sequence: 1,
//!     at_ms: 0,
//! })?;
//!
//! let x: &dyn Exchange = &paper;
//! let order = OrderRequest::market(ClientOrderId::generate("demo"), m, Side::Buy, OrderSize::Base(Qty::parse("0.5")?));
//! let ack = x.place_order(&order).await?;
//! assert_eq!(ack.status, OrderStatus::Filled);
//! # Ok::<_, ExchangeError>(())
//! # }).unwrap();
//! ```

#![forbid(unsafe_code)]

pub mod account;
pub mod cost;
pub mod credentials;
pub mod error;
pub mod exchange;
pub mod instrument;
pub mod market;
pub mod num;
pub mod order;
pub mod paper;
pub mod rate;

#[cfg(feature = "service")]
pub mod feeds;
#[cfg(feature = "vault")]
pub mod vault;

pub use account::{Balance, FeeTier, Fill, Liquidity, MarginSummary, Position, PositionBook};
pub use cost::{BookCostModel, Cost, CostModel};
pub use credentials::{Credential, EnvKind, Live, SecretSource, StaticSecret, Test};
pub use error::{ExchangeError, RejectReason, Result};
pub use exchange::{
    CandleQuery, Capabilities, Environment, EventStream, Exchange, FillQuery, MarketData, OrderQuery, OrderRef, Page, Preview, Trading, UserEvent,
};
pub use instrument::{AccountId, Asset, InstrumentKind, Market, Product, ProductId, ProductStatus, VenueId};
pub use market::{BookChange, BookDelta, BookSnapshot, Candle, CandleBuilder, Channel, Granularity, Level, MarketEvent, OrderBook, Ticker, Trade};
pub use num::{Decimal, Price, Qty, Rounding};
pub use order::{
    ClientOrderId, Order, OrderAck, OrderEvent, OrderId, OrderRequest, OrderSize, OrderStatus, OrderType, PositionSide, Side, TimeInForce,
};
pub use paper::{Paper, PaperOptions};
pub use rate::{BucketSpec, RateLimiter};

/// Milliseconds since the Unix epoch, from the system clock.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}
