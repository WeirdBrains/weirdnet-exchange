//! Orders: requests, acknowledgements, and the order state machine.
//!
//! ```text
//!            Accepted              Fill (part)            Fill (rest) / Done
//! Pending ───────────▶ Open ─────────────▶ PartiallyFilled ─────────────▶ Filled
//!    │                   │  CancelRequested         │
//!    │                   └──────────▶ CancelPending ◀┘── Cancelled ──▶ Cancelled
//!    ├── Rejected ──▶ Rejected        (Expired from any open state ──▶ Expired)
//! ```
//!
//! [`Order::apply`] enforces this, keeps the filled size, notional and fees, and ignores a
//! fill it has already seen (by trade id), so replays after a reconnect are harmless.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::account::Fill;
use crate::instrument::{Asset, Market};
use crate::num::{Decimal, Price, Qty};
use crate::{ExchangeError, RejectReason};

/// Buy or sell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// +1 for buys, -1 for sells.
    pub fn sign(self) -> Decimal {
        match self {
            Side::Buy => Decimal::ONE,
            Side::Sell => Decimal::NEGATIVE_ONE,
        }
    }
}

/// Long or short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSide {
    Long,
    Short,
}

/// How long an order stays on the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    /// Good till cancelled.
    Gtc,
    /// Good till a time.
    Gtd { expires_ms: i64 },
    /// Immediate or cancel: fill what can be filled now, cancel the rest.
    Ioc,
    /// Fill or kill: fill all of it now, or none.
    Fok,
}

/// The kind of order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OrderType {
    Market,
    /// `post_only` orders are refused instead of taking liquidity.
    Limit {
        price: Price,
        post_only: bool,
    },
    /// A limit order placed when the price reaches `stop`.
    StopLimit {
        stop: Price,
        limit: Price,
    },
    /// A market order placed when the price reaches `stop`.
    StopMarket {
        stop: Price,
    },
}

/// How much to trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSize {
    /// A base quantity.
    Base(Qty),
    /// A quote amount to spend or receive (market orders only).
    Quote(Decimal),
}

/// An id the client chooses for an order, so a retried placement is recognised by the venue
/// instead of placed twice. 1 to 64 characters of `A-Z a-z 0-9 - _`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientOrderId(String);

impl ClientOrderId {
    pub fn new(id: impl Into<String>) -> Result<Self, ExchangeError> {
        let id = id.into();
        if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(ExchangeError::invalid(format!("client order id {id:?}: 1 to 64 of A-Z a-z 0-9 - _")));
        }
        Ok(Self(id))
    }

    /// A fresh id: `prefix`, the time in milliseconds and a process counter.
    pub fn generate(prefix: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let p: String = prefix.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
        Self(format!("{p}-{:x}-{:x}", crate::now_ms(), SEQ.fetch_add(1, Ordering::Relaxed)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientOrderId {
    type Error = ExchangeError;
    fn try_from(s: String) -> Result<Self, ExchangeError> {
        Self::new(s)
    }
}

impl From<ClientOrderId> for String {
    fn from(c: ClientOrderId) -> String {
        c.0
    }
}

impl fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The venue's id for an order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(pub String);

impl OrderId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An order to place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub client_id: ClientOrderId,
    pub market: Market,
    pub side: Side,
    pub order_type: OrderType,
    pub size: OrderSize,
    pub time_in_force: TimeInForce,
    /// Only reduce an existing position (derivatives).
    #[serde(default)]
    pub reduce_only: bool,
}

impl OrderRequest {
    /// A market order (immediate or cancel).
    pub fn market(client_id: ClientOrderId, market: Market, side: Side, size: OrderSize) -> Self {
        Self { client_id, market, side, order_type: OrderType::Market, size, time_in_force: TimeInForce::Ioc, reduce_only: false }
    }

    /// A good-till-cancelled limit order.
    pub fn limit(client_id: ClientOrderId, market: Market, side: Side, price: Price, size: Qty) -> Self {
        Self {
            client_id,
            market,
            side,
            order_type: OrderType::Limit { price, post_only: false },
            size: OrderSize::Base(size),
            time_in_force: TimeInForce::Gtc,
            reduce_only: false,
        }
    }

    /// Makes a limit order post-only.
    pub fn post_only(mut self) -> Self {
        if let OrderType::Limit { post_only, .. } = &mut self.order_type {
            *post_only = true;
        }
        self
    }

    pub fn with_time_in_force(mut self, tif: TimeInForce) -> Self {
        self.time_in_force = tif;
        self
    }

    pub fn reduce_only(mut self) -> Self {
        self.reduce_only = true;
        self
    }

    /// The limit price, for limit and stop-limit orders.
    pub fn limit_price(&self) -> Option<Price> {
        match self.order_type {
            OrderType::Limit { price, .. } => Some(price),
            OrderType::StopLimit { limit, .. } => Some(limit),
            _ => None,
        }
    }

    /// The stop price, for stop orders.
    pub fn stop_price(&self) -> Option<Price> {
        match self.order_type {
            OrderType::StopLimit { stop, .. } | OrderType::StopMarket { stop } => Some(stop),
            _ => None,
        }
    }

    pub fn is_post_only(&self) -> bool {
        matches!(self.order_type, OrderType::Limit { post_only: true, .. })
    }

    /// The base quantity, when the size is given in base.
    pub fn base_size(&self) -> Option<Qty> {
        match self.size {
            OrderSize::Base(q) => Some(q),
            OrderSize::Quote(_) => None,
        }
    }
}

/// Where an order is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Sent, not acknowledged yet.
    Pending,
    /// On the book, nothing filled.
    Open,
    PartiallyFilled,
    /// A cancel was requested; fills may still arrive.
    CancelPending,
    Filled,
    Cancelled,
    Rejected,
    Expired,
}

impl OrderStatus {
    /// No further changes.
    pub fn is_terminal(self) -> bool {
        matches!(self, OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected | OrderStatus::Expired)
    }

    /// Working on the venue (may still fill).
    pub fn is_working(self) -> bool {
        !self.is_terminal()
    }
}

/// What a venue says happened to an order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OrderEvent {
    Accepted {
        order_id: OrderId,
    },
    Fill(Fill),
    /// The venue reports the order complete (for quote-sized orders, whose base size is not
    /// known in advance).
    Done,
    CancelRequested,
    Cancelled,
    Rejected {
        reason: RejectReason,
    },
    Expired,
}

impl OrderEvent {
    fn name(&self) -> &'static str {
        match self {
            OrderEvent::Accepted { .. } => "accepted",
            OrderEvent::Fill(_) => "fill",
            OrderEvent::Done => "done",
            OrderEvent::CancelRequested => "cancel-requested",
            OrderEvent::Cancelled => "cancelled",
            OrderEvent::Rejected { .. } => "rejected",
            OrderEvent::Expired => "expired",
        }
    }
}

/// A venue's answer to a placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderAck {
    pub order_id: OrderId,
    pub client_id: ClientOrderId,
    pub status: OrderStatus,
    pub at_ms: i64,
}

/// An order and everything that happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    /// The venue's id, once acknowledged.
    pub id: Option<OrderId>,
    pub request: OrderRequest,
    pub status: OrderStatus,
    /// Base quantity filled.
    pub filled: Qty,
    /// Sum of price × size over the fills, in the quote asset.
    pub notional: Decimal,
    /// Fees charged so far, in `fee_asset`.
    pub fees: Decimal,
    pub fee_asset: Option<Asset>,
    /// Trade ids of the fills applied (replays are ignored).
    pub fill_ids: Vec<String>,
    pub reject: Option<RejectReason>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Order {
    /// A new order, `Pending`.
    pub fn new(request: OrderRequest, now_ms: i64) -> Self {
        Self {
            id: None,
            request,
            status: OrderStatus::Pending,
            filled: Qty::ZERO,
            notional: Decimal::ZERO,
            fees: Decimal::ZERO,
            fee_asset: None,
            fill_ids: Vec::new(),
            reject: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    /// Volume-weighted average fill price.
    pub fn average_price(&self) -> Option<Price> {
        (self.filled.is_positive()).then(|| Price(self.notional / self.filled.0))
    }

    /// Base quantity left, when the size is in base.
    pub fn remaining(&self) -> Option<Qty> {
        self.request.base_size().map(|q| q - self.filled)
    }

    /// Applies an event, returning the new status. Impossible events are errors and change
    /// nothing; a fill seen before (same trade id) is ignored.
    pub fn apply(&mut self, event: &OrderEvent, now_ms: i64) -> Result<OrderStatus, ExchangeError> {
        use OrderStatus::*;
        let bad = |from: OrderStatus, e: &OrderEvent| Err(ExchangeError::InvalidTransition { from, event: e.name().into() });
        let s = self.status;
        if let OrderEvent::Fill(f) = event
            && self.fill_ids.contains(&f.trade_id)
        {
            return Ok(s);
        }
        let next = match (s, event) {
            (Pending, OrderEvent::Accepted { order_id }) => {
                self.id = Some(order_id.clone());
                Open
            }
            (Open | PartiallyFilled | CancelPending, OrderEvent::Accepted { order_id }) if self.id.as_ref() == Some(order_id) => s,
            (Pending | Open | PartiallyFilled | CancelPending, OrderEvent::Fill(f)) => {
                if f.side != self.request.side || f.market != self.request.market {
                    return Err(ExchangeError::invalid(format!("fill {} is for another side or market", f.trade_id)));
                }
                if self.id.as_ref().is_some_and(|id| *id != f.order_id) {
                    return Err(ExchangeError::invalid(format!("fill {} is for order {}", f.trade_id, f.order_id)));
                }
                let filled = self.filled + f.size;
                if self.request.base_size().is_some_and(|q| filled > q) {
                    return bad(s, event);
                }
                self.id.get_or_insert_with(|| f.order_id.clone());
                self.filled = filled;
                self.notional += f.price * f.size;
                self.fees += f.fee;
                self.fee_asset.get_or_insert_with(|| f.fee_asset.clone());
                self.fill_ids.push(f.trade_id.clone());
                match self.request.base_size() {
                    Some(q) if filled == q => Filled,
                    _ if s == CancelPending => CancelPending,
                    _ => PartiallyFilled,
                }
            }
            (Open | PartiallyFilled | CancelPending, OrderEvent::Done) if self.filled.is_positive() => Filled,
            (Pending | Open | PartiallyFilled, OrderEvent::CancelRequested) => CancelPending,
            (CancelPending, OrderEvent::CancelRequested) => CancelPending,
            (Pending | Open | PartiallyFilled | CancelPending, OrderEvent::Cancelled) => Cancelled,
            (Pending | Open | PartiallyFilled | CancelPending, OrderEvent::Expired) => Expired,
            (Pending, OrderEvent::Rejected { reason }) => {
                self.reject = Some(*reason);
                Rejected
            }
            (Cancelled, OrderEvent::Cancelled) | (Expired, OrderEvent::Expired) | (Rejected, OrderEvent::Rejected { .. }) => s,
            _ => return bad(s, event),
        };
        self.status = next;
        self.updated_ms = now_ms;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Liquidity;

    fn req() -> OrderRequest {
        OrderRequest::limit(
            ClientOrderId::new("c1").unwrap(),
            Market::new("v", "BTC-USD"),
            Side::Buy,
            Price::parse("100").unwrap(),
            Qty::parse("2").unwrap(),
        )
    }

    fn fill(id: &str, size: &str) -> OrderEvent {
        OrderEvent::Fill(Fill {
            trade_id: id.into(),
            order_id: OrderId::new("o1"),
            client_id: None,
            market: Market::new("v", "BTC-USD"),
            side: Side::Buy,
            price: Price::parse("100").unwrap(),
            size: Qty::parse(size).unwrap(),
            fee: Decimal::from_str_exact("0.1").unwrap(),
            fee_asset: Asset::new("USD"),
            liquidity: Liquidity::Maker,
            at_ms: 0,
        })
    }

    #[test]
    fn the_happy_path_and_replays() {
        let mut o = Order::new(req(), 0);
        assert_eq!(o.apply(&OrderEvent::Accepted { order_id: OrderId::new("o1") }, 1).unwrap(), OrderStatus::Open);
        assert_eq!(o.apply(&fill("t1", "0.5"), 2).unwrap(), OrderStatus::PartiallyFilled);
        assert_eq!(o.apply(&fill("t1", "0.5"), 3).unwrap(), OrderStatus::PartiallyFilled, "replayed fill ignored");
        assert_eq!(o.filled, Qty::parse("0.5").unwrap());
        assert_eq!(o.apply(&fill("t2", "1.5"), 4).unwrap(), OrderStatus::Filled);
        assert_eq!(
            (o.average_price(), o.fees, o.remaining()),
            (Some(Price::parse("100").unwrap()), Decimal::from_str_exact("0.2").unwrap(), Some(Qty::ZERO))
        );
        assert!(o.status.is_terminal());
        assert!(o.apply(&OrderEvent::Cancelled, 5).is_err());
        assert!(o.apply(&fill("t3", "0.1"), 5).is_err());
    }

    #[test]
    fn cancels_rejections_and_impossible_events() {
        let mut o = Order::new(req(), 0);
        assert!(o.apply(&OrderEvent::Done, 1).is_err(), "nothing filled");
        o.apply(&OrderEvent::Accepted { order_id: OrderId::new("o1") }, 1).unwrap();
        assert_eq!(o.apply(&OrderEvent::CancelRequested, 2).unwrap(), OrderStatus::CancelPending);
        assert_eq!(o.apply(&fill("t1", "1"), 3).unwrap(), OrderStatus::CancelPending, "fills race cancels");
        assert_eq!(o.apply(&OrderEvent::Cancelled, 4).unwrap(), OrderStatus::Cancelled);
        assert_eq!(o.apply(&OrderEvent::Cancelled, 5).unwrap(), OrderStatus::Cancelled, "idempotent");

        let mut o = Order::new(req(), 0);
        assert!(o.apply(&fill("t9", "3"), 1).is_err(), "overfill");
        assert_eq!(o.status, OrderStatus::Pending, "unchanged on error");
        assert_eq!(o.apply(&OrderEvent::Rejected { reason: RejectReason::InsufficientFunds }, 1).unwrap(), OrderStatus::Rejected);
        assert_eq!(o.reject, Some(RejectReason::InsufficientFunds));
        let mut o = Order::new(req(), 0);
        o.apply(&OrderEvent::Accepted { order_id: OrderId::new("o1") }, 1).unwrap();
        assert!(o.apply(&OrderEvent::Rejected { reason: RejectReason::Other }, 2).is_err());
        assert!(o.apply(&OrderEvent::Accepted { order_id: OrderId::new("other") }, 2).is_err());
    }

    #[test]
    fn client_ids_validate() {
        assert!(ClientOrderId::new("ok_id-1").is_ok());
        for bad in ["", "has space", &"x".repeat(65)] {
            assert!(ClientOrderId::new(bad).is_err());
        }
        let a = ClientOrderId::generate("dry run!");
        assert!(a.as_str().starts_with("dryrun-") && ClientOrderId::new(a.as_str()).is_ok());
        assert_ne!(a, ClientOrderId::generate("dry run!"));
        assert!(serde_json::from_str::<ClientOrderId>("\"bad id\"").is_err());
    }
}
