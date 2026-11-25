//! [`Paper`]: an in-memory exchange for tests and dry runs.
//!
//! It implements [`MarketData`] and [`Trading`] in the [`Environment::Test`] environment with
//! no network. You give it products, balances, books and trades; it matches orders against
//! them:
//!
//! - Market and marketable limit orders walk the book as takers and use up its liquidity.
//! - Post-only orders that would take are rejected; IOC and market orders expire whatever
//!   doesn't fill at once; FOK orders fill in full or expire.
//! - GTC and GTD limit orders rest. They fill as makers, at their own price, when a book or
//!   delta crosses them or a recorded trade trades through or at their price. This is
//!   optimistic: there is no queue position. GTD orders expire on the paper clock.
//! - Spot orders need the funds: a resting buy holds its quote (with the larger fee rate), a
//!   resting sell holds its base. Fills move balances and charge fees in the quote asset.
//! - Derivative products track positions: realized PnL less fees settles into the quote
//!   balance. Margin is not modelled.
//! - Placing is idempotent per client order id.
//!
//! Stop orders are not supported. The clock is the system clock until [`Paper::set_time`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::broadcast;

use crate::account::{Balance, FeeTier, Fill, Liquidity, Position, PositionBook};
use crate::exchange::{
    CandleQuery, Capabilities, Environment, EventStream, FillQuery, MarketData, OrderQuery, OrderRef, Page, Preview, Trading, UserEvent,
};
use crate::instrument::{Asset, Market, Product, ProductId, ProductStatus, VenueId};
use crate::market::{BookDelta, BookSnapshot, Candle, Channel, Granularity, MarketEvent, OrderBook, Ticker, Trade};
use crate::num::{Decimal, Price, Qty, Rounding, round_to};
use crate::order::{Order, OrderAck, OrderEvent, OrderId, OrderRequest, OrderSize, OrderType, Side, TimeInForce};
use crate::{ExchangeError, RejectReason, Result};

/// Settings for a [`Paper`] exchange.
#[derive(Debug, Clone)]
pub struct PaperOptions {
    pub venue: VenueId,
    pub fees: FeeTier,
    /// Events buffered per stream subscriber before it lags.
    pub buffer: usize,
    /// Recent trades kept per product.
    pub keep_trades: usize,
}

impl Default for PaperOptions {
    fn default() -> Self {
        Self { venue: VenueId::new("paper"), fees: FeeTier::default(), buffer: 1024, keep_trades: 1000 }
    }
}

/// An in-memory simulated exchange.
pub struct Paper {
    venue: VenueId,
    st: Mutex<St>,
    user: broadcast::Sender<UserEvent>,
    market: broadcast::Sender<MarketEvent>,
}

struct St {
    products: BTreeMap<ProductId, Product>,
    books: HashMap<ProductId, OrderBook>,
    trades: HashMap<ProductId, VecDeque<Trade>>,
    candles: HashMap<(ProductId, Granularity), BTreeMap<i64, Candle>>,
    last: HashMap<ProductId, Price>,
    balances: BTreeMap<Asset, Balance>,
    orders: Vec<Order>,
    /// What each order holds (quote for buys, base for sells), parallel to `orders`.
    reserved: Vec<Decimal>,
    by_id: HashMap<OrderId, usize>,
    by_client: HashMap<crate::order::ClientOrderId, usize>,
    fills: Vec<Fill>,
    positions: PositionBook,
    fees: FeeTier,
    clock: Option<i64>,
    seq: u64,
    keep_trades: usize,
    user: broadcast::Sender<UserEvent>,
    market: broadcast::Sender<MarketEvent>,
}

impl Default for Paper {
    fn default() -> Self {
        Self::new(PaperOptions::default())
    }
}

impl Paper {
    pub fn new(options: PaperOptions) -> Self {
        let (user, _) = broadcast::channel(options.buffer.max(1));
        let (market, _) = broadcast::channel(options.buffer.max(1));
        let st = St {
            products: BTreeMap::new(),
            books: HashMap::new(),
            trades: HashMap::new(),
            candles: HashMap::new(),
            last: HashMap::new(),
            balances: BTreeMap::new(),
            orders: Vec::new(),
            reserved: Vec::new(),
            by_id: HashMap::new(),
            by_client: HashMap::new(),
            fills: Vec::new(),
            positions: PositionBook::new(),
            fees: options.fees,
            clock: None,
            seq: 0,
            keep_trades: options.keep_trades.max(1),
            user: user.clone(),
            market: market.clone(),
        };
        Self { venue: options.venue, st: Mutex::new(st), user, market }
    }

    /// A shared handle, for `Arc<dyn Exchange>`.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn lock(&self) -> MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A market on this venue.
    pub fn market(&self, product: &str) -> Market {
        Market { venue: self.venue.clone(), product: ProductId::new(product) }
    }

    /// Adds or replaces a product. Its market's venue must be this venue.
    pub fn add_product(&self, product: Product) -> Result<()> {
        if product.market.venue != self.venue {
            return Err(ExchangeError::invalid(format!("{} is not on {}", product.market, self.venue)));
        }
        let mut st = self.lock();
        let id = product.market.product.clone();
        st.books.entry(id.clone()).or_insert_with(|| OrderBook::new(product.market.clone()));
        st.products.insert(id, product);
        Ok(())
    }

    /// Changes a product's status (and tells status subscribers).
    pub fn set_status(&self, product: &ProductId, status: ProductStatus) -> Result<()> {
        let mut st = self.lock();
        let now = st.now();
        let p = st.products.get_mut(product).ok_or_else(|| ExchangeError::not_found(format!("product {product}")))?;
        p.status = status;
        let market = p.market.clone();
        let _ = st.market.send(MarketEvent::Status { market, status, at_ms: now });
        Ok(())
    }

    /// Adds to an asset's available balance (negative to withdraw).
    pub fn deposit(&self, asset: &str, amount: Decimal) {
        let mut st = self.lock();
        let a = Asset::new(asset);
        st.balance(&a).available += amount;
        st.emit_balance(&a);
    }

    pub fn set_fee_tier(&self, fees: FeeTier) {
        self.lock().fees = fees;
    }

    /// Fixes the clock at `ms` and expires GTD orders due by then.
    pub fn set_time(&self, ms: i64) {
        let mut st = self.lock();
        st.clock = Some(ms);
        st.expire_due();
    }

    /// Moves the clock on by `ms` (from the system time if it wasn't set).
    pub fn advance(&self, ms: i64) {
        let mut st = self.lock();
        st.clock = Some(st.now() + ms);
        st.expire_due();
    }

    pub fn now(&self) -> i64 {
        self.lock().now()
    }

    /// Replaces a product's book, then fills resting orders it crosses.
    pub fn set_book(&self, snapshot: BookSnapshot) -> Result<()> {
        let mut st = self.lock();
        let id = st.known(&snapshot.market)?;
        st.books.insert(id.clone(), OrderBook::from_snapshot(&snapshot));
        let _ = st.market.send(MarketEvent::Book(snapshot));
        st.match_book(&id);
        st.emit_ticker(&id);
        Ok(())
    }

    /// Applies a delta to a product's book (gap-checked), then fills resting orders it crosses.
    pub fn apply_delta(&self, delta: BookDelta) -> Result<()> {
        let mut st = self.lock();
        let id = st.known(&delta.market)?;
        let book = st.books.entry(id.clone()).or_insert_with(|| OrderBook::new(delta.market.clone()));
        if book.apply(&delta)? {
            let _ = st.market.send(MarketEvent::BookDelta(delta));
            st.match_book(&id);
            st.emit_ticker(&id);
        }
        Ok(())
    }

    /// Records a public trade: keeps it, updates candles, and fills resting orders it trades
    /// through or at.
    pub fn record_trade(&self, trade: Trade) -> Result<()> {
        let mut st = self.lock();
        let id = st.known(&trade.market)?;
        st.last.insert(id.clone(), trade.price);
        let keep = st.keep_trades;
        let q = st.trades.entry(id.clone()).or_default();
        if q.len() == keep {
            q.pop_front();
        }
        q.push_back(trade.clone());
        for g in Granularity::ALL {
            let start = g.start_of(trade.at_ms);
            let series = st.candles.entry((id.clone(), g)).or_default();
            let c = series.entry(start).or_insert_with(|| Candle {
                market: trade.market.clone(),
                granularity: g,
                start_ms: start,
                open: trade.price,
                high: trade.price,
                low: trade.price,
                close: trade.price,
                volume: Qty::ZERO,
            });
            c.high = c.high.max(trade.price);
            c.low = c.low.min(trade.price);
            c.close = trade.price;
            c.volume = c.volume + trade.size;
            let c = c.clone();
            let _ = st.market.send(MarketEvent::Candle(c));
        }
        let _ = st.market.send(MarketEvent::Trade(trade.clone()));
        st.match_trade(&id, &trade);
        Ok(())
    }

    /// Adds historical candles (for candle queries in backtests).
    pub fn add_candles(&self, candles: impl IntoIterator<Item = Candle>) {
        let mut st = self.lock();
        for c in candles {
            st.candles.entry((c.market.product.clone(), c.granularity)).or_default().insert(c.start_ms, c);
        }
    }
}

fn reject<T>(reason: RejectReason, message: impl Into<String>) -> Result<T> {
    Err(ExchangeError::rejected(reason, message))
}

/// What a taker would fill walking `book`: (price, size) per level.
fn plan(book: &OrderBook, side: Side, limit: Option<Price>, size: OrderSize, size_increment: Decimal) -> Vec<(Price, Qty)> {
    let mut out = Vec::new();
    let (mut base_left, mut quote_left) = match size {
        OrderSize::Base(q) => (Some(q), None),
        OrderSize::Quote(a) => (None, Some(a)),
    };
    for l in book.levels(side.opposite()) {
        if limit.is_some_and(|lim| if side == Side::Buy { l.price > lim } else { l.price < lim }) {
            break;
        }
        let take = match (base_left, quote_left) {
            (Some(b), _) => b.min(l.size),
            (_, Some(a)) => l.size.min(Qty(round_to(a / l.price.0, size_increment, Rounding::Down))),
            _ => break,
        };
        if !take.is_positive() {
            break;
        }
        out.push((l.price, take));
        if let Some(b) = &mut base_left {
            *b = *b - take;
        }
        if let Some(a) = &mut quote_left {
            *a -= l.price * take;
        }
    }
    out
}

fn totals(p: &[(Price, Qty)]) -> (Qty, Decimal) {
    p.iter().fold((Qty::ZERO, Decimal::ZERO), |(q, n), (px, sz)| (q + *sz, n + *px * *sz))
}

fn rests(r: &OrderRequest) -> bool {
    matches!(r.order_type, OrderType::Limit { .. }) && matches!(r.time_in_force, TimeInForce::Gtc | TimeInForce::Gtd { .. })
}

impl St {
    fn now(&self) -> i64 {
        self.clock.unwrap_or_else(crate::now_ms)
    }

    fn known(&self, market: &Market) -> Result<ProductId> {
        match self.products.get(&market.product) {
            Some(p) if p.market == *market => Ok(market.product.clone()),
            _ => Err(ExchangeError::not_found(format!("product {market}"))),
        }
    }

    fn balance(&mut self, a: &Asset) -> &mut Balance {
        self.balances.entry(a.clone()).or_insert_with(|| Balance { asset: a.clone(), available: Decimal::ZERO, hold: Decimal::ZERO })
    }

    fn emit_balance(&self, a: &Asset) {
        if let Some(b) = self.balances.get(a) {
            let _ = self.user.send(UserEvent::Balance(b.clone()));
        }
    }

    fn emit_order(&self, i: usize) {
        let _ = self.user.send(UserEvent::Order(self.orders[i].clone()));
    }

    fn emit_ticker(&self, id: &ProductId) {
        if let Ok(t) = self.ticker(id) {
            let _ = self.market.send(MarketEvent::Ticker(t));
        }
    }

    fn ticker(&self, id: &ProductId) -> Result<Ticker> {
        let p = self.products.get(id).ok_or_else(|| ExchangeError::not_found(format!("product {id}")))?;
        let book = self.books.get(id);
        Ok(Ticker {
            market: p.market.clone(),
            bid: book.and_then(|b| b.best_bid()),
            ask: book.and_then(|b| b.best_ask()),
            last: self.last.get(id).copied(),
            volume_24h: None,
            at_ms: self.now(),
        })
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("paper-{prefix}{}", self.seq)
    }

    fn find(&self, r: &OrderRef) -> Result<usize> {
        match r {
            OrderRef::Id(id) => self.by_id.get(id),
            OrderRef::Client(c) => self.by_client.get(c),
        }
        .copied()
        .ok_or_else(|| ExchangeError::not_found(format!("order {r:?}")))
    }

    fn ack(&self, i: usize) -> OrderAck {
        let o = &self.orders[i];
        OrderAck {
            order_id: o.id.clone().unwrap_or_else(|| OrderId::new("")),
            client_id: o.request.client_id.clone(),
            status: o.status,
            at_ms: o.created_ms,
        }
    }

    fn max_rate(&self) -> Decimal {
        self.fees.maker.max(self.fees.taker)
    }

    /// The checks a venue makes before accepting an order, and what would fill at once.
    fn vet(&self, r: &OrderRequest) -> Result<(Product, Vec<(Price, Qty)>)> {
        let product = match self.products.get(&r.market.product) {
            Some(p) if p.market == r.market => p.clone(),
            _ => return reject(RejectReason::UnknownProduct, format!("{} is not listed", r.market)),
        };
        paper_capabilities().check(r)?;
        if matches!(r.time_in_force, TimeInForce::Fok) && r.base_size().is_none() {
            return reject(RejectReason::UnsupportedOrder, "fill-or-kill orders must be sized in base");
        }
        product.check_order(r)?;
        if let TimeInForce::Gtd { expires_ms } = r.time_in_force
            && expires_ms <= self.now()
        {
            return Err(ExchangeError::invalid("good-till-date is in the past"));
        }
        if r.reduce_only {
            let pos = self.positions.get(&r.market).map(|p| p.size).unwrap_or_default();
            let size = r.base_size().map(|q| q.0).unwrap_or(Decimal::MAX);
            if !product.kind.is_derivative() || pos.is_zero() || (pos > Decimal::ZERO) == (r.side == Side::Buy) || size > pos.abs() {
                return reject(RejectReason::ReduceOnly, "a reduce-only order must shrink an open position");
            }
        }
        let empty = OrderBook::new(r.market.clone());
        let book = self.books.get(&r.market.product).unwrap_or(&empty);
        let fills = plan(book, r.side, r.limit_price(), r.size, product.size_increment);
        if r.is_post_only() && !fills.is_empty() {
            return reject(RejectReason::PostOnlyWouldTake, "post-only order would take liquidity");
        }
        let (qty, notional) = totals(&fills);
        let full = r.base_size().is_some_and(|q| qty == q);
        if matches!(r.time_in_force, TimeInForce::Fok) && !full {
            return Ok((product, Vec::new()));
        }
        let quote = self.balances.get(&product.quote).map(|b| b.available).unwrap_or_default();
        let taker_cost = notional * (Decimal::ONE + self.fees.taker);
        if product.kind.is_derivative() {
            if quote < notional * self.fees.taker {
                return reject(RejectReason::InsufficientFunds, format!("fees need {} {}", notional * self.fees.taker, product.quote));
            }
        } else if r.side == Side::Buy {
            let resting = match (rests(r), r.limit_price(), r.base_size()) {
                (true, Some(p), Some(q)) => p * (q - qty) * (Decimal::ONE + self.max_rate()),
                _ => Decimal::ZERO,
            };
            if quote < taker_cost + resting {
                return reject(RejectReason::InsufficientFunds, format!("needs {} {}, has {quote}", taker_cost + resting, product.quote));
            }
        } else {
            let base = self.balances.get(&product.base).map(|b| b.available).unwrap_or_default();
            let need = r.base_size().map(|q| q.0).unwrap_or(qty.0);
            if base < need {
                return reject(RejectReason::InsufficientFunds, format!("needs {need} {}, has {base}", product.base));
            }
        }
        Ok((product, fills))
    }

    fn place(&mut self, r: &OrderRequest) -> Result<OrderAck> {
        self.expire_due();
        if let Some(&i) = self.by_client.get(&r.client_id) {
            if self.orders[i].request == *r {
                return Ok(self.ack(i));
            }
            return reject(RejectReason::DuplicateClientId, format!("client id {} was used for another order", r.client_id));
        }
        let (product, fills) = self.vet(r)?;
        let now = self.now();
        let id = OrderId::new(self.next_id("o"));
        let mut order = Order::new(r.clone(), now);
        order.apply(&OrderEvent::Accepted { order_id: id.clone() }, now)?;
        let i = self.orders.len();
        self.orders.push(order);
        self.reserved.push(Decimal::ZERO);
        self.by_id.insert(id, i);
        self.by_client.insert(r.client_id.clone(), i);

        let id_ = r.market.product.clone();
        for (price, size) in fills {
            if let Some(book) = self.books.get_mut(&id_) {
                let left = book.levels(r.side.opposite()).find(|l| l.price == price).map(|l| l.size).unwrap_or_default();
                book.set(r.side.opposite(), price, left - size);
            }
            self.fill(i, &product, price, size, Liquidity::Taker);
        }
        let o = &self.orders[i];
        if o.status.is_working() {
            if o.request.base_size().is_none() {
                let e = if o.filled.is_positive() { OrderEvent::Done } else { OrderEvent::Expired };
                self.orders[i].apply(&e, now)?;
            } else if rests(r) {
                self.reserve(i, &product);
            } else {
                self.orders[i].apply(&OrderEvent::Expired, now)?;
            }
        }
        self.emit_order(i);
        Ok(self.ack(i))
    }

    /// Holds what a resting order needs.
    fn reserve(&mut self, i: usize, p: &Product) {
        if p.kind.is_derivative() {
            return;
        }
        let o = &self.orders[i];
        let remaining = o.remaining().unwrap_or_default();
        let (asset, amount) = match (o.request.side, o.request.limit_price()) {
            (Side::Buy, Some(px)) => (p.quote.clone(), px * remaining * (Decimal::ONE + self.max_rate())),
            (Side::Sell, _) => (p.base.clone(), remaining.0),
            _ => return,
        };
        let b = self.balance(&asset);
        b.available -= amount;
        b.hold += amount;
        self.reserved[i] = amount;
        self.emit_balance(&asset);
    }

    /// Returns up to `amount` of what order `i` holds (all of it for `None`).
    fn release(&mut self, i: usize, amount: Option<Decimal>) {
        let held = self.reserved[i];
        let amount = amount.map_or(held, |a| a.min(held));
        if amount.is_zero() {
            return;
        }
        let Some(p) = self.products.get(&self.orders[i].request.market.product) else { return };
        let asset = if self.orders[i].request.side == Side::Buy { p.quote.clone() } else { p.base.clone() };
        let b = self.balance(&asset);
        b.hold -= amount;
        b.available += amount;
        self.reserved[i] -= amount;
    }

    /// Fills `size` of order `i` at `price` and settles balances and positions.
    fn fill(&mut self, i: usize, p: &Product, price: Price, size: Qty, liquidity: Liquidity) {
        let now = self.now();
        let notional = price * size;
        let fee = self.fees.fee(liquidity, notional);
        let o = &self.orders[i];
        let f = Fill {
            trade_id: format!("paper-t{}", self.seq + 1),
            order_id: o.id.clone().unwrap_or_else(|| OrderId::new("")),
            client_id: Some(o.request.client_id.clone()),
            market: o.request.market.clone(),
            side: o.request.side,
            price,
            size,
            fee,
            fee_asset: p.quote.clone(),
            liquidity,
            at_ms: now,
        };
        self.seq += 1;
        if self.orders[i].apply(&OrderEvent::Fill(f.clone()), now).is_err() {
            return;
        }
        let side = f.side;
        if p.kind.is_derivative() {
            let before = self.positions.get(&f.market).map(|x| x.realized_pnl).unwrap_or_default();
            let after = self.positions.apply(&f, &p.quote).realized_pnl;
            self.balance(&p.quote).available += after - before - fee;
        } else {
            self.positions.apply(&f, &p.quote);
            match side {
                Side::Buy => {
                    if let Some(px) = self.orders[i].request.limit_price() {
                        self.release(i, Some(px * size * (Decimal::ONE + self.max_rate())));
                    }
                    self.balance(&p.quote).available -= notional + fee;
                    self.balance(&p.base).available += size.0;
                }
                Side::Sell => {
                    self.release(i, Some(size.0));
                    self.balance(&p.base).available -= size.0;
                    self.balance(&p.quote).available += notional - fee;
                }
            }
        }
        if self.orders[i].status.is_terminal() {
            self.release(i, None);
        }
        self.fills.push(f.clone());
        let _ = self.user.send(UserEvent::Fill(f));
        self.emit_balance(&p.base);
        self.emit_balance(&p.quote);
        if p.kind.is_derivative()
            && let Some(pos) = self.positions.get(&p.market)
        {
            let _ = self.user.send(UserEvent::Position(pos.clone()));
        }
    }

    fn resting(&self, id: &ProductId) -> Vec<usize> {
        (0..self.orders.len()).filter(|&i| self.orders[i].request.market.product == *id && self.orders[i].status.is_working()).collect()
    }

    /// Fills resting orders the book now crosses.
    fn match_book(&mut self, id: &ProductId) {
        let Some(product) = self.products.get(id).cloned() else { return };
        for i in self.resting(id) {
            let side = self.orders[i].request.side;
            let Some(limit) = self.orders[i].request.limit_price() else { continue };
            loop {
                let remaining = self.orders[i].remaining().unwrap_or_default();
                let Some(book) = self.books.get_mut(id) else { break };
                let Some(best) = book.levels(side.opposite()).next() else { break };
                let crosses = if side == Side::Buy { best.price <= limit } else { best.price >= limit };
                if !remaining.is_positive() || !crosses || !self.orders[i].status.is_working() {
                    break;
                }
                let take = remaining.min(best.size);
                book.set(side.opposite(), best.price, best.size - take);
                self.fill(i, &product, limit, take, Liquidity::Maker);
            }
            self.emit_order(i);
        }
    }

    /// Fills resting orders a trade went through or at.
    fn match_trade(&mut self, id: &ProductId, t: &Trade) {
        let Some(product) = self.products.get(id).cloned() else { return };
        let mut left = t.size;
        for i in self.resting(id) {
            if !left.is_positive() {
                break;
            }
            let side = self.orders[i].request.side;
            let Some(limit) = self.orders[i].request.limit_price() else { continue };
            let hit = if side == Side::Buy { t.price <= limit } else { t.price >= limit };
            let remaining = self.orders[i].remaining().unwrap_or_default();
            if hit && remaining.is_positive() {
                let take = remaining.min(left);
                left = left - take;
                self.fill(i, &product, limit, take, Liquidity::Maker);
                self.emit_order(i);
            }
        }
    }

    fn end(&mut self, i: usize, event: OrderEvent) -> Result<()> {
        let now = self.now();
        let o = &mut self.orders[i];
        if o.status.is_terminal() {
            return Err(ExchangeError::invalid(format!("order is already {:?}", o.status)));
        }
        if event == OrderEvent::Cancelled {
            o.apply(&OrderEvent::CancelRequested, now)?;
        }
        o.apply(&event, now)?;
        self.release(i, None);
        if let Some(p) = self.products.get(&self.orders[i].request.market.product) {
            let (b, q) = (p.base.clone(), p.quote.clone());
            self.emit_balance(&b);
            self.emit_balance(&q);
        }
        self.emit_order(i);
        Ok(())
    }

    fn expire_due(&mut self) {
        let now = self.now();
        let due: Vec<usize> = (0..self.orders.len())
            .filter(|&i| {
                self.orders[i].status.is_working()
                    && matches!(self.orders[i].request.time_in_force, TimeInForce::Gtd { expires_ms } if expires_ms <= now)
            })
            .collect();
        for i in due {
            let _ = self.end(i, OrderEvent::Expired);
        }
    }
}

fn paper_capabilities() -> Capabilities {
    let mut c = Capabilities::all();
    c.stop_orders = false;
    c.margin = false;
    c
}

/// Items from index `cursor` on, `limit` of them, with the next cursor.
fn page<T: Clone>(items: Vec<(usize, &T)>, cursor: Option<&str>, limit: Option<usize>) -> Result<Page<T>> {
    let from = match cursor {
        Some(c) => c.parse::<usize>().map_err(|_| ExchangeError::invalid(format!("cursor {c:?}")))?,
        None => 0,
    };
    let limit = limit.unwrap_or(100).max(1);
    let mut it = items.into_iter().filter(|(i, _)| *i >= from);
    let out: Vec<T> = it.by_ref().take(limit).map(|(_, t)| t.clone()).collect();
    Ok(Page { items: out, next: it.next().map(|(i, _)| i.to_string()) })
}

fn in_range(at: i64, start: Option<i64>, end: Option<i64>) -> bool {
    start.is_none_or(|s| at >= s) && end.is_none_or(|e| at < e)
}

fn broadcast_stream<T: Clone + Send + 'static>(
    initial: Vec<T>,
    rx: broadcast::Receiver<T>,
    keep: impl Fn(&T) -> bool + Send + Sync + 'static,
) -> EventStream<T> {
    let keep = Arc::new(keep);
    let live = futures::stream::unfold(rx, move |mut rx| {
        let keep = keep.clone();
        async move {
            loop {
                match rx.recv().await {
                    Ok(e) if keep(&e) => return Some((Ok(e), rx)),
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        return Some((Err(ExchangeError::transient(format!("lagged by {n} events"), false)), rx));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    });
    futures::stream::iter(initial.into_iter().map(Ok)).chain(live).boxed()
}

#[async_trait]
impl MarketData for Paper {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn products(&self) -> Result<Vec<Product>> {
        Ok(self.lock().products.values().cloned().collect())
    }

    async fn ticker(&self, product: &ProductId) -> Result<Ticker> {
        self.lock().ticker(product)
    }

    async fn book(&self, product: &ProductId, depth: Option<usize>) -> Result<BookSnapshot> {
        let st = self.lock();
        st.books.get(product).map(|b| b.snapshot(depth)).ok_or_else(|| ExchangeError::not_found(format!("product {product}")))
    }

    async fn trades(&self, product: &ProductId, limit: usize) -> Result<Vec<Trade>> {
        let st = self.lock();
        if !st.products.contains_key(product) {
            return Err(ExchangeError::not_found(format!("product {product}")));
        }
        Ok(st.trades.get(product).map(|q| q.iter().rev().take(limit).cloned().collect()).unwrap_or_default())
    }

    async fn candles(&self, q: &CandleQuery) -> Result<Page<Candle>> {
        let st = self.lock();
        if !st.products.contains_key(&q.product) {
            return Err(ExchangeError::not_found(format!("product {}", q.product)));
        }
        let from = match &q.cursor {
            Some(c) => c.parse::<i64>().map_err(|_| ExchangeError::invalid(format!("cursor {c:?}")))?,
            None => q.start_ms,
        };
        let limit = q.limit.unwrap_or(300).max(1);
        let Some(series) = st.candles.get(&(q.product.clone(), q.granularity)) else {
            return Ok(Page { items: Vec::new(), next: None });
        };
        let mut it = series.range(from.max(q.start_ms)..q.end_ms.max(q.start_ms));
        let items: Vec<Candle> = it.by_ref().take(limit).map(|(_, c)| c.clone()).collect();
        Ok(Page { items, next: it.next().map(|(s, _)| s.to_string()) })
    }

    async fn server_time(&self) -> Result<i64> {
        Ok(self.now())
    }

    async fn subscribe(&self, products: &[ProductId], channels: &[Channel]) -> Result<EventStream<MarketEvent>> {
        let st = self.lock();
        for p in products {
            if !st.products.contains_key(p) {
                return Err(ExchangeError::not_found(format!("product {p}")));
            }
        }
        let rx = self.market.subscribe();
        let wanted = |id: &ProductId| products.is_empty() || products.contains(id);
        let initial: Vec<MarketEvent> = if channels.contains(&Channel::Book) {
            st.books.iter().filter(|(id, _)| wanted(id)).map(|(_, b)| MarketEvent::Book(b.snapshot(None))).collect()
        } else {
            Vec::new()
        };
        let products = products.to_vec();
        let channels = channels.to_vec();
        Ok(broadcast_stream(initial, rx, move |e| {
            let Some(m) = e.market() else { return false };
            if !(products.is_empty() || products.contains(&m.product)) {
                return false;
            }
            channels.iter().any(|c| match (c, e) {
                (Channel::Trades, MarketEvent::Trade(_))
                | (Channel::Ticker, MarketEvent::Ticker(_))
                | (Channel::Status, MarketEvent::Status { .. }) => true,
                (Channel::Book, MarketEvent::Book(_) | MarketEvent::BookDelta(_)) => true,
                (Channel::Candles(g), MarketEvent::Candle(c)) => c.granularity == *g,
                _ => false,
            })
        }))
    }
}

#[async_trait]
impl Trading for Paper {
    fn environment(&self) -> Environment {
        Environment::Test
    }

    fn capabilities(&self) -> Capabilities {
        paper_capabilities()
    }

    async fn balances(&self) -> Result<Vec<Balance>> {
        Ok(self.lock().balances.values().cloned().collect())
    }

    async fn fee_tier(&self) -> Result<FeeTier> {
        Ok(self.lock().fees)
    }

    async fn preview_order(&self, r: &OrderRequest) -> Result<Preview> {
        let st = self.lock();
        let (_, fills) = st.vet(r)?;
        let (size, notional) = totals(&fills);
        let mut warnings = Vec::new();
        if r.base_size().is_some_and(|q| size < q) {
            warnings.push(if rests(r) { "only part fills at once; the rest rests".into() } else { "only part fills; the rest expires".into() });
        }
        Ok(Preview {
            size,
            notional,
            average_price: size.is_positive().then(|| Price(notional / size.0)),
            fee: st.fees.fee(Liquidity::Taker, notional),
            warnings,
        })
    }

    async fn place_order(&self, r: &OrderRequest) -> Result<OrderAck> {
        self.lock().place(r)
    }

    async fn cancel(&self, r: &OrderRef) -> Result<()> {
        let mut st = self.lock();
        let i = st.find(r)?;
        st.end(i, OrderEvent::Cancelled)
    }

    async fn cancel_all(&self, product: Option<&ProductId>) -> Result<Vec<OrderId>> {
        let mut st = self.lock();
        let working: Vec<usize> = (0..st.orders.len())
            .filter(|&i| st.orders[i].status.is_working() && product.is_none_or(|p| st.orders[i].request.market.product == *p))
            .collect();
        let mut ids = Vec::new();
        for i in working {
            st.end(i, OrderEvent::Cancelled)?;
            ids.extend(st.orders[i].id.clone());
        }
        Ok(ids)
    }

    async fn order(&self, r: &OrderRef) -> Result<Order> {
        let mut st = self.lock();
        st.expire_due();
        let i = st.find(r)?;
        Ok(st.orders[i].clone())
    }

    async fn orders(&self, q: &OrderQuery) -> Result<Page<Order>> {
        let mut st = self.lock();
        st.expire_due();
        let items = st
            .orders
            .iter()
            .enumerate()
            .filter(|(_, o)| q.product.as_ref().is_none_or(|p| o.request.market.product == *p))
            .filter(|(_, o)| !q.open_only || o.status.is_working())
            .filter(|(_, o)| in_range(o.created_ms, q.start_ms, q.end_ms))
            .collect();
        page(items, q.cursor.as_deref(), q.limit)
    }

    async fn fills(&self, q: &FillQuery) -> Result<Page<Fill>> {
        let st = self.lock();
        let items = st
            .fills
            .iter()
            .enumerate()
            .filter(|(_, f)| q.product.as_ref().is_none_or(|p| f.market.product == *p))
            .filter(|(_, f)| q.order_id.as_ref().is_none_or(|id| f.order_id == *id))
            .filter(|(_, f)| in_range(f.at_ms, q.start_ms, q.end_ms))
            .collect();
        page(items, q.cursor.as_deref(), q.limit)
    }

    async fn positions(&self) -> Result<Vec<Position>> {
        let st = self.lock();
        Ok(st.positions.open().filter(|p| st.products.get(&p.market.product).is_some_and(|x| x.kind.is_derivative())).cloned().collect())
    }

    async fn subscribe_user(&self) -> Result<EventStream<UserEvent>> {
        Ok(broadcast_stream(Vec::new(), self.user.subscribe(), |_| true))
    }
}
