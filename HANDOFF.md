# Handoff

Running notes for [weirdnet-exchange](https://github.com/WeirdBrains/weirdnet-exchange), newest first.

## 2026-11-25: initial crate, unreleased

- What changed: the first implementation. Exact decimal `Price` and `Qty`; `Market`,
  `Product` and `InstrumentKind` (spot, dated future, perpetual) with order checks; orders
  (`Side`, `OrderType`, `TimeInForce`, `OrderSize`, `ClientOrderId`, `OrderRequest`) and the
  strict `Order` state machine; `Fill`, `Balance`, `FeeTier`, `Position`, `PositionBook`,
  `MarginSummary`; `Trade`, `Ticker`, `OrderBook` with snapshot, delta and gap checks,
  `Candle` and `CandleBuilder`; the `MarketData`, `Trading` and `Exchange` traits with
  `Capabilities` and paging; `ExchangeError` and `RejectReason`; `BookCostModel`;
  `RateLimiter`; `Credential<Live>` and `Credential<Test>`; the in-memory `Paper` exchange; the
  `vault` feature (`VaultSecret`, [weirdnet-vault](https://crates.io/crates/weirdnet-vault)
  0.1) and the `service` feature (`MarketHubs`,
  [weirdnet-service](https://crates.io/crates/weirdnet-service) 0.1). No exchange adapters and
  no network calls. Version 0.0.0 until the first cut (0.1.0).
- Commits: `df02393` (crate), `13f9410` (docs), `48e4159` (CI and release config), and this
  entry.
- Checks: `cargo fmt --check`; `cargo clippy --all-targets -D warnings` with default,
  no-default, `vault`, `service` and all features; `cargo test` with default, no-default and
  all features (11 unit tests, 6 paper-exchange tests, a vault test, a hub test and 2 doc
  tests); `cargo doc --no-deps --all-features` with `-D warnings`; `cargo package --locked`;
  `weirdnet_release.py validate`.
- Not tested: any real venue (there are no adapters); the paper exchange under concurrent load;
  vault-backed credentials with a file-backed vault or the OS keychain (tests use an in-memory
  vault); hubs served over weirdnet-net to remote subscribers; Windows and macOS until the first
  CI run.
- Left: CI on GitHub; the 0.1.0 release.
- Next: venue adapters in their own crates, built on these traits.

---

A [Weird Brains](https://weirdbrains.com) project.
