//! Market data as `weirdnet-service` hub topics (feature `service`).
//!
//! [`MarketHubs`] keeps one hub per kind of data, keyed by [`Market`]: books (a snapshot, then
//! [`BookDelta`]s), recent trades, tickers and candles. Feed a market-data stream into it with
//! [`MarketHubs::publish`] or [`MarketHubs::forward`] and serve the hubs with
//! `weirdnet_service::serve_feed`; subscribers get a snapshot, then deltas, and can resume.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use weirdnet_service::{Hub, HubOptions, State};

use crate::exchange::EventStream;
use crate::instrument::Market;
use crate::market::{BookDelta, Candle, Granularity, MarketEvent, OrderBook, Ticker, Trade};
use crate::{ExchangeError, Result};

impl State for OrderBook {
    type Delta = BookDelta;

    /// Applies the delta's changes if it is newer than the book. Gaps are caught before
    /// publishing (see [`MarketHubs::publish`]).
    fn apply(&mut self, d: &BookDelta) {
        if d.sequence > self.sequence {
            for c in &d.changes {
                self.set(c.side, c.price, c.size);
            }
            self.sequence = d.sequence;
            self.updated_ms = d.at_ms;
        }
    }
}

impl State for Ticker {
    type Delta = Ticker;

    fn apply(&mut self, d: &Ticker) {
        *self = d.clone();
    }

    fn coalesce_key(_: &Ticker) -> Option<String> {
        Some(String::new())
    }
}

/// The latest trades in a market, oldest first, up to `capacity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentTrades {
    pub market: Market,
    pub capacity: usize,
    pub trades: VecDeque<Trade>,
}

impl RecentTrades {
    pub fn new(market: Market, capacity: usize) -> Self {
        Self { market, capacity: capacity.max(1), trades: VecDeque::new() }
    }
}

impl State for RecentTrades {
    type Delta = Trade;

    fn apply(&mut self, t: &Trade) {
        if self.trades.len() == self.capacity {
            self.trades.pop_front();
        }
        self.trades.push_back(t.clone());
    }
}

/// A market's candles at one granularity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CandleKey {
    pub market: Market,
    pub granularity: Granularity,
}

/// Candles oldest first, up to `capacity`. A candle with a start already present replaces it
/// (the current candle updating).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandleSeries {
    pub key: CandleKey,
    pub capacity: usize,
    pub candles: VecDeque<Candle>,
}

impl CandleSeries {
    pub fn new(key: CandleKey, capacity: usize) -> Self {
        Self { key, capacity: capacity.max(1), candles: VecDeque::new() }
    }
}

impl State for CandleSeries {
    type Delta = Candle;

    fn apply(&mut self, c: &Candle) {
        match self.candles.iter_mut().rev().find(|x| x.start_ms == c.start_ms) {
            Some(x) => *x = c.clone(),
            None => {
                let at = self.candles.iter().position(|x| x.start_ms > c.start_ms).unwrap_or(self.candles.len());
                self.candles.insert(at, c.clone());
                if self.candles.len() > self.capacity {
                    self.candles.pop_front();
                }
            }
        }
    }

    fn coalesce_key(c: &Candle) -> Option<String> {
        Some(c.start_ms.to_string())
    }
}

/// Hubs for books, trades, tickers and candles.
pub struct MarketHubs {
    pub books: Arc<Hub<Market, OrderBook>>,
    pub trades: Arc<Hub<Market, RecentTrades>>,
    pub tickers: Arc<Hub<Market, Ticker>>,
    pub candles: Arc<Hub<CandleKey, CandleSeries>>,
    /// Trades and candles kept per topic.
    pub keep: usize,
    sequences: Mutex<HashMap<Market, u64>>,
}

impl MarketHubs {
    pub fn new(options: HubOptions) -> Self {
        Self {
            books: Arc::new(Hub::new(options.clone())),
            trades: Arc::new(Hub::new(options.clone())),
            tickers: Arc::new(Hub::new(options.clone())),
            candles: Arc::new(Hub::new(options)),
            keep: 500,
            sequences: Mutex::new(HashMap::new()),
        }
    }

    /// Publishes one event. A book delta for a market with no snapshot yet, or one that skips
    /// a sequence number, is a [`ExchangeError::BookGap`] and is not published: subscribe
    /// again for a snapshot. Expects one publisher per market.
    pub fn publish(&self, event: &MarketEvent) -> Result<()> {
        let svc = |e: weirdnet_service::ServiceError| ExchangeError::invalid(e.to_string());
        match event {
            MarketEvent::Book(s) => {
                self.books.insert(s.market.clone(), OrderBook::from_snapshot(s));
                self.sequences.lock().unwrap_or_else(|e| e.into_inner()).insert(s.market.clone(), s.sequence);
            }
            MarketEvent::BookDelta(d) => {
                let mut seqs = self.sequences.lock().unwrap_or_else(|e| e.into_inner());
                let Some(cur) = seqs.get_mut(&d.market) else {
                    return Err(ExchangeError::BookGap { expected: 0, got: d.sequence });
                };
                if d.sequence <= *cur {
                    return Ok(());
                }
                if d.sequence != *cur + 1 {
                    return Err(ExchangeError::BookGap { expected: *cur + 1, got: d.sequence });
                }
                self.books.publish(&d.market, d.clone()).map_err(svc)?;
                *cur = d.sequence;
            }
            MarketEvent::Trade(t) => {
                if !self.trades.contains(&t.market) {
                    self.trades.insert(t.market.clone(), RecentTrades::new(t.market.clone(), self.keep));
                }
                self.trades.publish(&t.market, t.clone()).map_err(svc)?;
            }
            MarketEvent::Ticker(t) => {
                if self.tickers.contains(&t.market) {
                    self.tickers.publish(&t.market, t.clone()).map_err(svc)?;
                } else {
                    self.tickers.insert(t.market.clone(), t.clone());
                }
            }
            MarketEvent::Candle(c) => {
                let key = CandleKey { market: c.market.clone(), granularity: c.granularity };
                if !self.candles.contains(&key) {
                    self.candles.insert(key.clone(), CandleSeries::new(key.clone(), self.keep));
                }
                self.candles.publish(&key, c.clone()).map_err(svc)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Publishes a stream until it ends (`Ok`) or fails: an error item or a book gap is
    /// returned so the caller can subscribe again.
    pub async fn forward(&self, mut events: EventStream<MarketEvent>) -> Result<()> {
        while let Some(e) = events.next().await {
            self.publish(&e?)?;
        }
        Ok(())
    }
}

impl Default for MarketHubs {
    fn default() -> Self {
        Self::new(HubOptions::default())
    }
}
