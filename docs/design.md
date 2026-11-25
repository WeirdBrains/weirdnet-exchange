# Design notes

How [weirdnet-exchange](https://github.com/WeirdBrains/weirdnet-exchange) is put together and
why. The API reference is on [docs.rs](https://docs.rs/weirdnet-exchange).

## Numbers

Prices and sizes are decimals, never floats: `0.1 + 0.2` is `0.3`, and a size rounded to an
increment of `0.0001` is exactly that. `Price` and `Qty` are separate types so one can't be
passed for the other; `Price * Qty` gives the notional as a plain `Decimal`. They serialize as
strings, which is what venues send and what JSON can carry without loss.

Timestamps are `i64` milliseconds since the Unix epoch, UTC.

## Orders

`OrderRequest` is what an app asks for; `Order` is what happened. `Order::apply` takes the
venue's events (accepted, fill, cancel requested, cancelled, rejected, expired, done) and moves
through the states drawn in the `order` module docs. It refuses an event that is impossible in
the current state, and a fill that would take the order past its size, without changing
anything. Fills carry trade ids; a fill seen before is ignored, so replaying a venue's fill
history after a reconnect is safe. Fills may still arrive while a cancel is pending.

Every request carries a `ClientOrderId`. `Trading::place_order` is idempotent per client id:
the same request again returns the first acknowledgement, and a different request with a used
id is rejected. When a placement times out, `ExchangeError::Transient { outcome_unknown: true
}` tells the app to look the order up by client id before trying again.

## Adapter traits

`MarketData` needs no credentials; `Trading` does. Splitting them lets a market-data service
run without any secrets. Both are object safe (`async-trait`), so apps hold `Arc<dyn
Exchange>`. Optional parts (previews, recent trades, positions, margin) have default methods
that return `Unsupported`, and `Capabilities` says up front what an adapter supports.

Streams are `BoxStream`s of `Result` items: an error item (such as a consumer that fell
behind) does not have to end the stream. A book subscription starts with a snapshot; deltas
carry sequence numbers and `OrderBook::apply` reports a gap instead of guessing, so the
consumer subscribes again for a new snapshot.

## Errors

`ExchangeError` sorts failures by what the caller should do: wait (`RateLimited`, with the
venue's retry-after), retry (`Transient`, `Unavailable`), check first (`Transient` with an
unknown outcome), fix the request (`Rejected` with a `RejectReason`, `Invalid`), or stop
(`Auth`, `Environment`). `is_retryable` encodes that.

## Credentials

Adapters get secrets through `SecretSource`, which also reports the secret's environment.
`Credential<E>` wraps a source after checking it is in environment `E`, and checks again on
every use. A live adapter's constructor takes `Credential<Live>`, so test credentials can't
build one, and a sandbox constructor taking `Credential<Test>` can't be handed live ones. The
markers are sealed.

With the `vault` feature, `VaultSecret` names a secret in a
[weirdnet-vault](https://crates.io/crates/weirdnet-vault) vault under the scope
`<account>/<venue>`. Its environment is the one fixed in the vault when the secret was
created. It uses the secret through the vault's `use_secret`, so the plaintext is opened only
for the call; the vault's grants and audit log apply. A test-bound caller is refused a live
secret before the vault is asked, and the vault refuses it again.

## Paper exchange

`Paper` implements both traits in memory, in the `Test` environment, for unit tests, examples
and dry runs. It matches against the books and trades it is given: takers walk the book and
use up its liquidity; resting orders fill as makers at their own price when a book crosses
them or a trade reaches them. That is optimistic (there is no queue), which suits dry runs
but overstates fills. Spot orders hold their funds; derivative products track positions and
settle realized PnL less fees into the quote balance. Stop orders and margin are not
simulated.

## Market-data hubs

With the `service` feature, `OrderBook` is a
[weirdnet-service](https://crates.io/crates/weirdnet-service) `State` whose deltas are
`BookDelta`s, and `MarketHubs` keeps hubs for books, recent trades, tickers and candles keyed
by market. `MarketHubs::publish` refuses a delta that skips a sequence number, so a hub's
subscribers never see a book with a hole in it. Tickers and candles coalesce, so a slow
subscriber gets the latest values instead of a backlog.

---

A [Weird Brains](https://weirdbrains.com) project.
