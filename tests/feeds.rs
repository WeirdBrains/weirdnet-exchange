//! Market data through weirdnet-service hubs.
#![cfg(feature = "service")]

use std::sync::Arc;

use weirdnet_exchange::feeds::{CandleKey, MarketHubs};
use weirdnet_exchange::*;
use weirdnet_service::Event;

fn p(s: &str) -> Price {
    Price::parse(s).unwrap()
}
fn q(s: &str) -> Qty {
    Qty::parse(s).unwrap()
}

#[tokio::test]
async fn paper_stream_into_hubs() {
    let paper = Paper::default();
    let m = paper.market("BTC-USD");
    paper.add_product(Product::spot(m.clone(), "BTC", "USD")).unwrap();
    let hubs = Arc::new(MarketHubs::default());
    let stream = paper.subscribe(&[], &[Channel::Book, Channel::Trades, Channel::Ticker, Channel::Candles(Granularity::OneMinute)]).await.unwrap();
    let fwd = {
        let hubs = hubs.clone();
        tokio::spawn(async move { hubs.forward(stream).await })
    };
    paper
        .set_book(BookSnapshot {
            market: m.clone(),
            bids: vec![Level::new(p("99"), q("1"))],
            asks: vec![Level::new(p("101"), q("1"))],
            sequence: 5,
            at_ms: 0,
        })
        .unwrap();
    paper
        .apply_delta(BookDelta {
            market: m.clone(),
            sequence: 6,
            changes: vec![BookChange { side: Side::Buy, price: p("100"), size: q("2") }],
            at_ms: 1,
        })
        .unwrap();
    paper
        .record_trade(Trade { market: m.clone(), trade_id: "1".into(), price: p("100"), size: q("0.5"), taker_side: Some(Side::Sell), at_ms: 60_000 })
        .unwrap();

    for _ in 0..100 {
        if hubs.trades.contains(&m) && hubs.books.snapshot(&m).is_some_and(|(_, b)| b.sequence == 6) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, book) = hubs.books.snapshot(&m).unwrap();
    assert_eq!(book.best_bid(), Some(Level::new(p("100"), q("2"))));
    let mut w = hubs.books.subscribe(&m, None).unwrap();
    assert!(matches!(w.next().await, Some(Event::Snapshot { state, .. }) if state.sequence == 6));
    assert_eq!(hubs.trades.snapshot(&m).unwrap().1.trades.len(), 1);
    assert_eq!(hubs.tickers.snapshot(&m).unwrap().1.bid.unwrap().price, p("100"));
    let key = CandleKey { market: m.clone(), granularity: Granularity::OneMinute };
    assert_eq!(hubs.candles.snapshot(&key).unwrap().1.candles[0].volume, q("0.5"));

    // A gap is refused and not published.
    let gap = MarketEvent::BookDelta(BookDelta { market: m.clone(), sequence: 9, changes: vec![], at_ms: 2 });
    assert_eq!(hubs.publish(&gap), Err(ExchangeError::BookGap { expected: 7, got: 9 }));
    let next = MarketEvent::BookDelta(BookDelta {
        market: m.clone(),
        sequence: 7,
        changes: vec![BookChange { side: Side::Sell, price: p("101"), size: Qty::ZERO }],
        at_ms: 3,
    });
    hubs.publish(&next).unwrap();
    assert!(matches!(w.next().await, Some(Event::Deltas { items, .. }) if items[0].1.sequence == 7));
    let unknown = MarketEvent::BookDelta(BookDelta { market: paper.market("ETH-USD"), sequence: 1, changes: vec![], at_ms: 0 });
    assert!(matches!(hubs.publish(&unknown), Err(ExchangeError::BookGap { .. })));
    fwd.abort();
}
