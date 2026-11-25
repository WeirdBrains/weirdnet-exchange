# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Exact decimal `Price` and `Qty` with increment rounding.
- Instruments: `Market`, `Product` with increments, limits, status and kind, and order checks.
- Orders: sides, types, time in force, client order ids, and an `Order` state machine.
- Accounts: fills, balances, fee tiers, positions with PnL, and margin summaries.
- Market data: trades, tickers, order book snapshots and deltas with gap checks, candles.
- `MarketData`, `Trading` and `Exchange` adapter traits with capabilities and paging.
- `CostModel` and `BookCostModel`; a shared token-bucket `RateLimiter`.
- Credentials typed by environment (`Credential<Live>`, `Credential<Test>`).
- `Paper`, an in-memory simulated exchange.
- `vault` feature: credentials from weirdnet-vault.
- `service` feature: market data as weirdnet-service hub topics.
