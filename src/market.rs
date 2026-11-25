//! Market data: trades, tickers, order books and candles.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ExchangeError;
use crate::instrument::{Market, ProductStatus};
use crate::num::{Decimal, Price, Qty};
use crate::order::Side;

/// A public trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trade {
    pub market: Market,
    pub trade_id: String,
    pub price: Price,
    pub size: Qty,
    /// The taker's side, when the venue says.
    pub taker_side: Option<Side>,
    pub at_ms: i64,
}

/// Best bid and ask and the last trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticker {
    pub market: Market,
    pub bid: Option<Level>,
    pub ask: Option<Level>,
    pub last: Option<Price>,
    pub volume_24h: Option<Qty>,
    pub at_ms: i64,
}

/// A price level: the size resting at a price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Level {
    pub price: Price,
    pub size: Qty,
}

impl Level {
    pub fn new(price: Price, size: Qty) -> Self {
        Self { price, size }
    }
}

/// A whole book at one sequence number. Bids best (highest) first, asks best (lowest) first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub market: Market,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub sequence: u64,
    pub at_ms: i64,
}

/// One level's new size; a zero size removes the level. `side` is the book side
/// ([`Side::Buy`] for bids).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookChange {
    pub side: Side,
    pub price: Price,
    pub size: Qty,
}

/// Changes to a book. `sequence` follows the snapshot's or the previous delta's by one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookDelta {
    pub market: Market,
    pub sequence: u64,
    pub changes: Vec<BookChange>,
    pub at_ms: i64,
}

/// A level-2 order book kept from a snapshot and deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBook {
    pub market: Market,
    bids: BTreeMap<Price, Qty>,
    asks: BTreeMap<Price, Qty>,
    pub sequence: u64,
    pub updated_ms: i64,
}

impl OrderBook {
    /// An empty book at sequence 0.
    pub fn new(market: Market) -> Self {
        Self { market, bids: BTreeMap::new(), asks: BTreeMap::new(), sequence: 0, updated_ms: 0 }
    }

    pub fn from_snapshot(s: &BookSnapshot) -> Self {
        let mut b = Self::new(s.market.clone());
        b.reset(s);
        b
    }

    /// Replaces the book with a snapshot.
    pub fn reset(&mut self, s: &BookSnapshot) {
        let keep = |l: &&Level| l.size.is_positive();
        self.bids = s.bids.iter().filter(keep).map(|l| (l.price, l.size)).collect();
        self.asks = s.asks.iter().filter(keep).map(|l| (l.price, l.size)).collect();
        self.sequence = s.sequence;
        self.updated_ms = s.at_ms;
    }

    /// Applies a delta. A delta at or before the current sequence is ignored (`Ok(false)`); one
    /// that skips a sequence is a [`ExchangeError::BookGap`] and changes nothing: fetch a new
    /// snapshot.
    pub fn apply(&mut self, d: &BookDelta) -> Result<bool, ExchangeError> {
        if d.market != self.market {
            return Err(ExchangeError::invalid(format!("delta for {} applied to {}", d.market, self.market)));
        }
        if d.sequence <= self.sequence {
            return Ok(false);
        }
        if d.sequence != self.sequence + 1 {
            return Err(ExchangeError::BookGap { expected: self.sequence + 1, got: d.sequence });
        }
        for c in &d.changes {
            self.set(c.side, c.price, c.size);
        }
        self.sequence = d.sequence;
        self.updated_ms = d.at_ms;
        Ok(true)
    }

    /// Sets one level without sequence checks; a zero size removes it.
    pub fn set(&mut self, side: Side, price: Price, size: Qty) {
        let m = self.side_mut(side);
        if size.is_positive() {
            m.insert(price, size);
        } else {
            m.remove(&price);
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut BTreeMap<Price, Qty> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// Levels on one side, best first.
    pub fn levels(&self, side: Side) -> Box<dyn Iterator<Item = Level> + '_> {
        match side {
            Side::Buy => Box::new(self.bids.iter().rev().map(|(p, q)| Level::new(*p, *q))),
            Side::Sell => Box::new(self.asks.iter().map(|(p, q)| Level::new(*p, *q))),
        }
    }

    pub fn best_bid(&self) -> Option<Level> {
        self.levels(Side::Buy).next()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.levels(Side::Sell).next()
    }

    pub fn mid(&self) -> Option<Price> {
        Some(Price((self.best_bid()?.price.0 + self.best_ask()?.price.0) / Decimal::TWO))
    }

    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()?.price.0 - self.best_bid()?.price.0)
    }

    /// The best bid is at or above the best ask (a stale or broken book).
    pub fn is_crossed(&self) -> bool {
        matches!((self.best_bid(), self.best_ask()), (Some(b), Some(a)) if b.price >= a.price)
    }

    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// What a taker on `side` would get for `size` walking the opposite side, up to `limit`
    /// when given: the size filled and its notional.
    pub fn walk(&self, side: Side, size: Qty, limit: Option<Price>) -> (Qty, Decimal) {
        let mut left = size;
        let mut notional = Decimal::ZERO;
        for l in self.levels(side.opposite()) {
            if !left.is_positive() || limit.is_some_and(|lim| crosses(side, lim, l.price)) {
                break;
            }
            let take = left.min(l.size);
            notional += l.price * take;
            left = left - take;
        }
        (size - left, notional)
    }

    /// A snapshot of the top `depth` levels on each side (all levels for `None`).
    pub fn snapshot(&self, depth: Option<usize>) -> BookSnapshot {
        let n = depth.unwrap_or(usize::MAX);
        BookSnapshot {
            market: self.market.clone(),
            bids: self.levels(Side::Buy).take(n).collect(),
            asks: self.levels(Side::Sell).take(n).collect(),
            sequence: self.sequence,
            at_ms: self.updated_ms,
        }
    }
}

/// A level at `level` is beyond a taker's `limit` on `side`.
fn crosses(side: Side, limit: Price, level: Price) -> bool {
    match side {
        Side::Buy => level > limit,
        Side::Sell => level < limit,
    }
}

/// Candle widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    OneMinute,
    FiveMinutes,
    FifteenMinutes,
    OneHour,
    SixHours,
    OneDay,
}

impl Granularity {
    pub const ALL: [Granularity; 6] = [Self::OneMinute, Self::FiveMinutes, Self::FifteenMinutes, Self::OneHour, Self::SixHours, Self::OneDay];

    pub fn ms(self) -> i64 {
        const M: i64 = 60_000;
        match self {
            Self::OneMinute => M,
            Self::FiveMinutes => 5 * M,
            Self::FifteenMinutes => 15 * M,
            Self::OneHour => 60 * M,
            Self::SixHours => 360 * M,
            Self::OneDay => 1440 * M,
        }
    }

    /// The start of the candle holding `at_ms`.
    pub fn start_of(self, at_ms: i64) -> i64 {
        at_ms.div_euclid(self.ms()) * self.ms()
    }
}

/// Open, high, low, close and volume over one interval starting at `start_ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candle {
    pub market: Market,
    pub granularity: Granularity,
    pub start_ms: i64,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Qty,
}

impl Candle {
    fn from_trade(t: &Trade, g: Granularity) -> Self {
        Self {
            market: t.market.clone(),
            granularity: g,
            start_ms: g.start_of(t.at_ms),
            open: t.price,
            high: t.price,
            low: t.price,
            close: t.price,
            volume: t.size,
        }
    }

    fn add(&mut self, t: &Trade) {
        self.high = self.high.max(t.price);
        self.low = self.low.min(t.price);
        self.close = t.price;
        self.volume = self.volume + t.size;
    }
}

/// Builds candles from trades in time order.
#[derive(Debug, Clone)]
pub struct CandleBuilder {
    granularity: Granularity,
    current: Option<Candle>,
}

impl CandleBuilder {
    pub fn new(granularity: Granularity) -> Self {
        Self { granularity, current: None }
    }

    /// Adds a trade. Returns the previous candle when this trade starts a new one. Trades older
    /// than the current candle are dropped.
    pub fn push(&mut self, t: &Trade) -> Option<Candle> {
        let start = self.granularity.start_of(t.at_ms);
        match &mut self.current {
            Some(c) if c.start_ms == start => {
                c.add(t);
                None
            }
            Some(c) if c.start_ms > start => None,
            _ => self.current.replace(Candle::from_trade(t, self.granularity)),
        }
    }

    /// The candle being built.
    pub fn current(&self) -> Option<&Candle> {
        self.current.as_ref()
    }
}

/// Something a market-data stream delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MarketEvent {
    Trade(Trade),
    Ticker(Ticker),
    /// A full book; replaces what came before.
    Book(BookSnapshot),
    BookDelta(BookDelta),
    Candle(Candle),
    Status {
        market: Market,
        status: ProductStatus,
        at_ms: i64,
    },
    Heartbeat {
        at_ms: i64,
    },
}

impl MarketEvent {
    pub fn market(&self) -> Option<&Market> {
        match self {
            MarketEvent::Trade(t) => Some(&t.market),
            MarketEvent::Ticker(t) => Some(&t.market),
            MarketEvent::Book(b) => Some(&b.market),
            MarketEvent::BookDelta(d) => Some(&d.market),
            MarketEvent::Candle(c) => Some(&c.market),
            MarketEvent::Status { market, .. } => Some(market),
            MarketEvent::Heartbeat { .. } => None,
        }
    }
}

/// What to subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Trades,
    Ticker,
    /// A snapshot, then deltas.
    Book,
    Candles(Granularity),
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Price {
        Price::parse(s).unwrap()
    }
    fn q(s: &str) -> Qty {
        Qty::parse(s).unwrap()
    }
    fn m() -> Market {
        Market::new("v", "BTC-USD")
    }

    fn book() -> OrderBook {
        OrderBook::from_snapshot(&BookSnapshot {
            market: m(),
            bids: vec![Level::new(p("99"), q("1")), Level::new(p("98"), q("2"))],
            asks: vec![Level::new(p("101"), q("1")), Level::new(p("102"), q("2")), Level::new(p("103"), q("0"))],
            sequence: 10,
            at_ms: 0,
        })
    }

    #[test]
    fn book_snapshot_delta_gap_and_walk() {
        let mut b = book();
        assert_eq!(
            (b.best_bid().unwrap().price, b.best_ask().unwrap().price, b.mid(), b.spread()),
            (p("99"), p("101"), Some(p("100")), Some(Decimal::TWO))
        );
        assert_eq!(b.levels(Side::Sell).count(), 2, "zero levels dropped");
        let d = |seq, changes| BookDelta { market: m(), sequence: seq, changes, at_ms: seq as i64 };
        assert!(!b.apply(&d(10, vec![])).unwrap(), "stale ignored");
        assert!(
            b.apply(&d(
                11,
                vec![
                    BookChange { side: Side::Sell, price: p("101"), size: Qty::ZERO },
                    BookChange { side: Side::Buy, price: p("100"), size: q("3") }
                ]
            ))
            .unwrap()
        );
        assert_eq!((b.best_bid().unwrap(), b.best_ask().unwrap().price), (Level::new(p("100"), q("3")), p("102")));
        assert_eq!(b.apply(&d(13, vec![])), Err(ExchangeError::BookGap { expected: 12, got: 13 }));
        assert_eq!(b.sequence, 11);

        let b = book();
        assert_eq!(b.walk(Side::Buy, q("2"), None), (q("2"), Decimal::from(203)));
        assert_eq!(b.walk(Side::Buy, q("5"), None), (q("3"), Decimal::from(305)));
        assert_eq!(b.walk(Side::Buy, q("5"), Some(p("101"))), (q("1"), Decimal::from(101)));
        assert_eq!(b.walk(Side::Sell, q("2"), None), (q("2"), Decimal::from(197)));
        assert!(!b.is_crossed());
        let s = b.snapshot(Some(1));
        assert_eq!((s.bids.len(), s.asks.len(), s.sequence), (1, 1, 10));
        assert_eq!(OrderBook::from_snapshot(&b.snapshot(None)), b);
    }

    #[test]
    fn candles_from_trades() {
        let t = |id: &str, price: &str, at| Trade { market: m(), trade_id: id.into(), price: p(price), size: q("1"), taker_side: None, at_ms: at };
        let g = Granularity::OneMinute;
        let mut cb = CandleBuilder::new(g);
        assert_eq!(cb.push(&t("1", "10", 60_000)), None);
        assert_eq!(cb.push(&t("2", "12", 61_000)), None);
        assert_eq!(cb.push(&t("3", "9", 62_000)), None);
        assert_eq!(cb.push(&t("x", "1", 1_000)), None, "late trade dropped");
        let done = cb.push(&t("4", "11", 125_000)).unwrap();
        assert_eq!((done.start_ms, done.open, done.high, done.low, done.close, done.volume), (60_000, p("10"), p("12"), p("9"), p("9"), q("3")));
        assert_eq!(cb.current().unwrap().start_ms, 120_000);
        assert_eq!(Granularity::OneDay.start_of(-1), -86_400_000);
    }
}
