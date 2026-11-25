//! The paper exchange through the adapter traits.

use std::sync::Arc;

use futures::StreamExt;
use weirdnet_exchange::*;

fn d(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap()
}
fn p(s: &str) -> Price {
    Price::parse(s).unwrap()
}
fn q(s: &str) -> Qty {
    Qty::parse(s).unwrap()
}
fn cid(s: &str) -> ClientOrderId {
    ClientOrderId::new(s).unwrap()
}

fn setup() -> (Arc<Paper>, Market) {
    let paper = Paper::new(PaperOptions { fees: FeeTier::from_bps(10, 20), ..Default::default() }).shared();
    paper.set_time(1_000_000);
    let m = paper.market("BTC-USD");
    paper.add_product(Product::spot(m.clone(), "BTC", "USD").with_increments(d("0.01"), d("0.0001")).with_limits(q("0.001"), None, d("1"))).unwrap();
    paper.deposit("USD", d("10000"));
    paper.deposit("BTC", d("1"));
    paper
        .set_book(BookSnapshot {
            market: m.clone(),
            bids: vec![Level::new(p("99"), q("1")), Level::new(p("98"), q("2"))],
            asks: vec![Level::new(p("101"), q("1")), Level::new(p("102"), q("2"))],
            sequence: 1,
            at_ms: 1_000_000,
        })
        .unwrap();
    (paper, m)
}

async fn bal(x: &dyn Exchange, a: &str) -> Balance {
    x.balances().await.unwrap().into_iter().find(|b| b.asset.as_str() == a).unwrap()
}

#[tokio::test]
async fn market_buy_walks_the_book_and_charges_taker_fees() {
    let (paper, m) = setup();
    let x: Arc<dyn Exchange> = paper.clone();
    assert_eq!(x.environment(), Environment::Test);
    let r = OrderRequest::market(cid("m1"), m.clone(), Side::Buy, OrderSize::Base(q("2")));
    let pv = x.preview_order(&r).await.unwrap();
    assert_eq!((pv.size, pv.notional, pv.average_price, pv.fee), (q("2"), d("203"), Some(p("101.5")), d("0.406")));
    let ack = x.place_order(&r).await.unwrap();
    assert_eq!(ack.status, OrderStatus::Filled);
    assert_eq!(x.place_order(&r).await.unwrap(), ack, "idempotent per client id");
    let o = x.order(&OrderRef::Client(cid("m1"))).await.unwrap();
    assert_eq!((o.filled, o.notional, o.fees), (q("2"), d("203"), d("0.406")));
    assert_eq!(bal(&*x, "USD").await.available, d("9796.594"));
    assert_eq!(bal(&*x, "BTC").await.available, d("3"));
    let book = x.book(&m.product, None).await.unwrap();
    assert_eq!(book.asks, vec![Level::new(p("102"), q("1"))], "liquidity used up");
    let fills = x.fills(&FillQuery::default()).await.unwrap();
    assert_eq!(fills.items.len(), 2);
    assert!(fills.items.iter().all(|f| f.liquidity == Liquidity::Taker && f.client_id == Some(cid("m1"))));

    let other = OrderRequest::market(cid("m1"), m.clone(), Side::Sell, OrderSize::Base(q("1")));
    assert_eq!(x.place_order(&other).await.unwrap_err().reject_reason(), Some(RejectReason::DuplicateClientId));
}

#[tokio::test]
async fn quote_sized_ioc_and_fok() {
    let (paper, m) = setup();
    let ack = paper.place_order(&OrderRequest::market(cid("q1"), m.clone(), Side::Buy, OrderSize::Quote(d("50.5")))).await.unwrap();
    assert_eq!(ack.status, OrderStatus::Filled);
    assert_eq!(paper.order(&ack.order_id.clone().into()).await.unwrap().filled, q("0.5"));

    let ioc = OrderRequest::limit(cid("i1"), m.clone(), Side::Sell, p("99"), q("2")).with_time_in_force(TimeInForce::Ioc);
    paper.deposit("BTC", d("5"));
    let ack = paper.place_order(&ioc).await.unwrap();
    let o = paper.order(&ack.order_id.into()).await.unwrap();
    assert_eq!((o.status, o.filled), (OrderStatus::Expired, q("1")), "the rest expired");

    let fok = OrderRequest::limit(cid("f1"), m.clone(), Side::Buy, p("102"), q("5")).with_time_in_force(TimeInForce::Fok);
    let ack = paper.place_order(&fok).await.unwrap();
    assert_eq!(ack.status, OrderStatus::Expired);
    assert_eq!(paper.order(&ack.order_id.into()).await.unwrap().filled, Qty::ZERO);
}

#[tokio::test]
async fn resting_orders_hold_funds_fill_as_makers_and_cancel() {
    let (paper, m) = setup();
    let x: &dyn Exchange = &*paper;
    let mut user = x.subscribe_user().await.unwrap();
    let post = OrderRequest::limit(cid("p0"), m.clone(), Side::Buy, p("101"), q("1")).post_only();
    assert_eq!(x.place_order(&post).await.unwrap_err().reject_reason(), Some(RejectReason::PostOnlyWouldTake));

    let r = OrderRequest::limit(cid("r1"), m.clone(), Side::Buy, p("100"), q("2")).post_only();
    let ack = x.place_order(&r).await.unwrap();
    assert_eq!(ack.status, OrderStatus::Open);
    let usd = bal(x, "USD").await;
    assert_eq!((usd.available, usd.hold), (d("9799.6"), d("200.4")), "100 × 2 plus 0.2% held");

    // A trade through 100 fills 0.5 of it as a maker, at 100.
    paper
        .record_trade(Trade {
            market: m.clone(),
            trade_id: "t".into(),
            price: p("99.5"),
            size: q("0.5"),
            taker_side: Some(Side::Sell),
            at_ms: 1_000_100,
        })
        .unwrap();
    let o = x.order(&ack.order_id.clone().into()).await.unwrap();
    assert_eq!((o.status, o.filled, o.fees), (OrderStatus::PartiallyFilled, q("0.5"), d("0.05")));

    // The ask moving down to 100 fills another 1.
    paper
        .apply_delta(BookDelta {
            market: m.clone(),
            sequence: 2,
            changes: vec![BookChange { side: Side::Sell, price: p("100"), size: q("1") }],
            at_ms: 1_000_200,
        })
        .unwrap();
    let o = x.order(&ack.order_id.clone().into()).await.unwrap();
    assert_eq!(o.filled, q("1.5"));
    assert_eq!(x.book(&m.product, None).await.unwrap().asks[0].price, p("101"), "the crossing level was used up");
    assert!(matches!(
        paper.apply_delta(BookDelta { market: m.clone(), sequence: 9, changes: vec![], at_ms: 0 }),
        Err(ExchangeError::BookGap { expected: 3, got: 9 })
    ));

    x.cancel(&ack.order_id.clone().into()).await.unwrap();
    assert!(x.cancel(&ack.order_id.clone().into()).await.is_err());
    let usd = bal(x, "USD").await;
    // 10000 − 150 − 0.15 in fees, nothing held.
    assert_eq!((usd.available, usd.hold), (d("9849.85"), d("0")));
    assert_eq!(bal(x, "BTC").await.available, d("2.5"));
    let o = x.order(&ack.order_id.clone().into()).await.unwrap();
    assert_eq!((o.status, o.average_price()), (OrderStatus::Cancelled, Some(p("100"))));

    let mut kinds = Vec::new();
    while let Some(Ok(e)) = futures::FutureExt::now_or_never(user.next()).flatten() {
        kinds.push(match e {
            UserEvent::Order(o) => format!("order:{:?}", o.status),
            UserEvent::Fill(f) => format!("fill:{}", f.size),
            UserEvent::Balance(_) => "balance".into(),
            _ => "other".into(),
        });
    }
    let interesting: Vec<_> = kinds.iter().filter(|k| !k.starts_with("balance")).cloned().collect();
    assert_eq!(interesting, ["order:Open", "fill:0.5", "order:PartiallyFilled", "fill:1", "order:PartiallyFilled", "order:Cancelled"]);
}

#[tokio::test]
async fn validation_funds_gtd_and_cancel_all() {
    let (paper, m) = setup();
    let x: &dyn Exchange = &*paper;
    let bad = OrderRequest::limit(cid("b1"), m.clone(), Side::Buy, p("90.001"), q("1"));
    assert_eq!(x.place_order(&bad).await.unwrap_err().reject_reason(), Some(RejectReason::BadIncrement));
    let small = OrderRequest::limit(cid("b2"), m.clone(), Side::Buy, p("90"), q("0.0001"));
    assert_eq!(x.place_order(&small).await.unwrap_err().reject_reason(), Some(RejectReason::BelowMinSize));
    let big = OrderRequest::limit(cid("b3"), m.clone(), Side::Buy, p("90"), q("200"));
    assert_eq!(x.place_order(&big).await.unwrap_err().reject_reason(), Some(RejectReason::InsufficientFunds));
    let oversell = OrderRequest::limit(cid("b4"), m.clone(), Side::Sell, p("200"), q("2"));
    assert_eq!(x.place_order(&oversell).await.unwrap_err().reject_reason(), Some(RejectReason::InsufficientFunds));
    let other = OrderRequest::limit(cid("b5"), paper.market("ETH-USD"), Side::Buy, p("90"), q("1"));
    assert_eq!(x.place_order(&other).await.unwrap_err().reject_reason(), Some(RejectReason::UnknownProduct));
    let stop = OrderRequest {
        order_type: OrderType::StopMarket { stop: p("120") },
        ..OrderRequest::market(cid("b6"), m.clone(), Side::Buy, OrderSize::Base(q("1")))
    };
    assert_eq!(x.place_order(&stop).await.unwrap_err().reject_reason(), Some(RejectReason::UnsupportedOrder));
    let reduce = OrderRequest::market(cid("b7"), m.clone(), Side::Sell, OrderSize::Base(q("1"))).reduce_only();
    assert_eq!(x.place_order(&reduce).await.unwrap_err().reject_reason(), Some(RejectReason::ReduceOnly));

    let gtd =
        OrderRequest::limit(cid("g1"), m.clone(), Side::Sell, p("150"), q("0.5")).with_time_in_force(TimeInForce::Gtd { expires_ms: 1_000_500 });
    let g = x.place_order(&gtd).await.unwrap();
    assert_eq!(bal(x, "BTC").await.hold, d("0.5"));
    let a = x.place_order(&OrderRequest::limit(cid("a1"), m.clone(), Side::Buy, p("90"), q("1"))).await.unwrap();
    let _b = x.place_order(&OrderRequest::limit(cid("a2"), m.clone(), Side::Buy, p("91"), q("1"))).await.unwrap();
    paper.advance(500);
    assert_eq!(x.order(&g.order_id.into()).await.unwrap().status, OrderStatus::Expired);
    assert_eq!(bal(x, "BTC").await.hold, d("0"));
    let open = x.orders(&OrderQuery { open_only: true, limit: Some(1), ..Default::default() }).await.unwrap();
    assert_eq!((open.items.len(), open.items[0].id.clone()), (1, Some(a.order_id.clone())));
    let rest = x.orders(&OrderQuery { open_only: true, cursor: open.next.clone(), ..Default::default() }).await.unwrap();
    assert_eq!((rest.items.len(), rest.next), (1, None));
    assert_eq!(x.cancel_all(Some(&m.product)).await.unwrap().len(), 2);
    let usd = bal(x, "USD").await;
    assert_eq!((usd.available, usd.hold), (d("10000"), d("0")));
}

#[tokio::test]
async fn perpetual_positions_settle_pnl() {
    let paper = Paper::default();
    paper.set_time(0);
    let m = paper.market("BTC-PERP");
    paper
        .add_product(
            Product::spot(m.clone(), "BTC", "USD")
                .with_kind(InstrumentKind::Perpetual { contract_size: Decimal::ONE, funding_interval_ms: 28_800_000 }),
        )
        .unwrap();
    paper.deposit("USD", d("1000"));
    let book = |bid: &str, ask: &str, seq| BookSnapshot {
        market: m.clone(),
        bids: vec![Level::new(p(bid), q("10"))],
        asks: vec![Level::new(p(ask), q("10"))],
        sequence: seq,
        at_ms: 0,
    };
    paper.set_book(book("99", "100", 1)).unwrap();
    paper.place_order(&OrderRequest::market(cid("l1"), m.clone(), Side::Buy, OrderSize::Base(q("2")))).await.unwrap();
    let pos = paper.positions().await.unwrap();
    assert_eq!((pos.len(), pos[0].size, pos[0].entry_price), (1, d("2"), Some(p("100"))));
    paper.set_book(book("110", "111", 2)).unwrap();
    assert!(paper.place_order(&OrderRequest::market(cid("l2"), m.clone(), Side::Buy, OrderSize::Base(q("1"))).reduce_only()).await.is_err());
    paper.place_order(&OrderRequest::market(cid("l3"), m.clone(), Side::Sell, OrderSize::Base(q("2"))).reduce_only()).await.unwrap();
    assert!(paper.positions().await.unwrap().is_empty());
    assert_eq!(bal(&paper, "USD").await.available, d("1020"), "20 realized, no fees");
}

#[tokio::test]
async fn market_streams_candles_and_trades() {
    let (paper, m) = setup();
    let mut s =
        paper.subscribe(std::slice::from_ref(&m.product), &[Channel::Book, Channel::Trades, Channel::Candles(Granularity::OneMinute)]).await.unwrap();
    assert!(matches!(s.next().await, Some(Ok(MarketEvent::Book(b))) if b.sequence == 1));
    let t =
        |id: &str, price: &str, at| Trade { market: m.clone(), trade_id: id.into(), price: p(price), size: q("0.1"), taker_side: None, at_ms: at };
    paper.record_trade(t("1", "100", 60_000)).unwrap();
    assert!(matches!(s.next().await, Some(Ok(MarketEvent::Candle(c))) if c.granularity == Granularity::OneMinute));
    assert!(matches!(s.next().await, Some(Ok(MarketEvent::Trade(x))) if x.trade_id == "1"));
    paper.apply_delta(BookDelta { market: m.clone(), sequence: 2, changes: vec![], at_ms: 0 }).unwrap();
    assert!(matches!(s.next().await, Some(Ok(MarketEvent::BookDelta(x))) if x.sequence == 2));
    paper.record_trade(t("2", "105", 61_000)).unwrap();
    paper.record_trade(t("3", "95", 130_000)).unwrap();
    let page = paper
        .candles(&CandleQuery {
            product: m.product.clone(),
            granularity: Granularity::OneMinute,
            start_ms: 0,
            end_ms: 200_000,
            limit: Some(1),
            cursor: None,
        })
        .await
        .unwrap();
    let c = &page.items[0];
    assert_eq!((c.start_ms, c.open, c.high, c.close, c.volume), (60_000, p("100"), p("105"), p("105"), q("0.2")));
    let next = paper
        .candles(&CandleQuery {
            product: m.product.clone(),
            granularity: Granularity::OneMinute,
            start_ms: 0,
            end_ms: 200_000,
            limit: Some(1),
            cursor: page.next,
        })
        .await
        .unwrap();
    assert_eq!((next.items[0].start_ms, next.next), (120_000, None));
    let recent = paper.trades(&m.product, 2).await.unwrap();
    assert_eq!(recent.iter().map(|t| t.trade_id.as_str()).collect::<Vec<_>>(), ["3", "2"]);
    let tk = paper.ticker(&m.product).await.unwrap();
    assert_eq!((tk.bid.unwrap().price, tk.ask.unwrap().price, tk.last), (p("99"), p("101"), Some(p("95"))));
    assert_eq!(paper.server_time().await.unwrap(), 1_000_000);
}
