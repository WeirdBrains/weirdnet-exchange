# weirdnet-exchange agents

The collective `AGENTS.md` in `WeirdBrains/weirdnet-agents` is mandatory and wins on conflict.

- Build outside the source checkout, with `CARGO_TARGET_DIR` on a scratch disk; clean targets afterwards.
- Exchange-neutral only: no venue adapters, no venue names in types, no network calls.
- No floats for prices, sizes, fees or balances; use `Decimal`, `Price` and `Qty`.
- Keep the test/live split: anything that builds a live adapter takes `Credential<Live>`.
- Never log, print, serialize or put secrets in errors; `Debug` on credentials stays redacted.
- `Order::apply` must stay strict: refuse impossible transitions and overfills, ignore replays.
- Keep `docs/design.md` in step with behaviour changes.
