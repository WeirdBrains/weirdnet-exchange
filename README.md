# weirdnet-exchange

Exchange-neutral trading types and traits for [Weird Brains](https://weirdbrains.com) apps on
Weirdnet. Adapters for real venues live in their own crates and implement the traits here;
apps talk to the traits and can swap in the in-memory `Paper` exchange for tests and dry runs.
This crate has no exchange adapters and makes no network calls.

- **Exact numbers.** `Price` and `Qty` wrap a decimal (no floats) and serialize as strings.
  Rounding to a product's increments is explicit (`Rounding::Down`, `Up`, `Nearest`).
- **Instruments.** `Market` (venue and product), `Product` with increments, size and notional
  limits, status, and kind (spot, dated future, perpetual). `Product::check_order` makes the
  checks a venue would.
- **Orders.** `Side`, `OrderType` (market, limit with post-only, stop-limit, stop-market),
  `TimeInForce` (GTC, GTD, IOC, FOK), base or quote sizes, validated `ClientOrderId`s, and an
  `Order` state machine that refuses impossible transitions and overfills and ignores
  replayed fills.
- **Accounts.** `Fill`, `Balance` (available and held), `FeeTier`, `Position` and
  `PositionBook` (weighted-average entry, realized and unrealized PnL, flips), `MarginSummary`.
- **Market data.** `Trade`, `Ticker`, `BookSnapshot` and `BookDelta`, an `OrderBook` that
  applies deltas and reports sequence gaps, `Candle` and `CandleBuilder`, and `MarketEvent`.
- **Adapter traits.** `MarketData` (public data and streams) and `Trading` (balances, fees,
  preview, idempotent placement, cancel, queries, positions and user streams), with
  `Capabilities`; `Exchange` is both. Errors say whether a retry is safe.
- **Helpers.** `CostModel` and `BookCostModel` (fees, spread, slippage, carry), and a shared
  token-bucket `RateLimiter` with retry-after back-off.
- **Credentials.** `Credential<Live>` and `Credential<Test>` are checked against their
  source's environment, so a live adapter can't be built from test credentials.
- **Paper exchange.** `Paper` matches market, limit, post-only, IOC, FOK, GTC and GTD orders
  against the books and trades you give it, holds funds, charges fees and tracks positions.

```rust
use weirdnet_exchange::*;

async fn demo() -> Result<()> {
    let paper = Paper::default();
    let m = paper.market("BTC-USD");
    paper.add_product(Product::spot(m.clone(), "BTC", "USD").with_increments(Decimal::new(1, 2), Decimal::new(1, 4)))?;
    paper.deposit("USD", Decimal::from(1000));
    paper.set_book(BookSnapshot {
        market: m.clone(),
        bids: vec![Level::new(Price::parse("99")?, Qty::parse("1")?)],
        asks: vec![Level::new(Price::parse("101")?, Qty::parse("1")?)],
        sequence: 1,
        at_ms: 0,
    })?;

    let x: &dyn Exchange = &paper;
    let order = OrderRequest::market(ClientOrderId::generate("demo"), m, Side::Buy, OrderSize::Base(Qty::parse("0.5")?));
    let ack = x.place_order(&order).await?;
    assert_eq!(ack.status, OrderStatus::Filled);
    Ok(())
}
```

The [design notes](docs/design.md) explain the choices. API documentation is on
[docs.rs](https://docs.rs/weirdnet-exchange).

## Features

| Feature | Default | Adds |
|---|---|---|
| `vault` | no | `vault::VaultSecret`: credentials from [weirdnet-vault](https://crates.io/crates/weirdnet-vault), scoped per account and venue, test and live kept apart |
| `service` | no | `feeds::MarketHubs`: books, trades, tickers and candles as [weirdnet-service](https://crates.io/crates/weirdnet-service) hub topics |

## Status

The API may change before 1.0. The paper exchange is a simulation: resting orders fill
optimistically (no queue position) and margin is not modelled.

## License

BSD-3-Clause, copyright Weird Brains LLC. See [LICENSE](LICENSE).

---

A [Weird Brains](https://weirdbrains.com) project.
