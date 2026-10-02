# kenoma-backtester

Standalone Rust research backtester for Kenoma pods.

V1 provides:

- A timestamp-ordered event clock for bars, quotes, trades, MBO, timers, orders, fills, and marks.
- Typed `Strategy` API with `StrategyContext` causal feature cutoff auditing. Strict mode rejects bars without an explicit `feature_cutoff_ts`.
- Conservative causal fills by default: no same-event market fills, quote and strictly-after-open bar execution, pessimistic bar bracket ambiguity, and MBO queue-ahead limit validation.
- Configurable bar-fill price model (default **worst_case**) so that bar-only feeds do not silently report fills at prices the bar never traded at — see [Bar fill modes](#bar-fill-modes).
- Multi-asset instrument metadata and portfolio accounting for equities, crypto spot/perps, futures, options, FX, fees, borrow, and funding.
- Built-in Black-Scholes-Merton, Black-76, CRR binomial, Greeks, and IV solver.
- DBN MBO ingest support behind the `kenoma-data/dbn` feature.
- Reproducible TOML manifests and structured run artifacts.

## Bar fill modes

Bar feeds carry only OHLCV — they do not say where in the bar a print
happened, only the boundary set. `[execution].bar_fill_mode` selects the
settlement convention:

| Mode | Market | Stop | Limit (touch) |
| --- | --- | --- | --- |
| `idealized` | `open ± half_spread` | `max(open, stop) ± half_spread` | fills at `limit_price` when bar wicks the level |
| `worst_case` (default) | `high + half_spread` (buy) / `low - half_spread` (sell) | `max(open, stop, high) + half_spread` (buy) / symmetric | fills on touch at `limit_price` |
| `print_through_limit` | same as `worst_case` | same as `worst_case` | requires bar to print *through* the level (low strictly below limit by ≥ ½ tick) before filling |

`idealized` credits fills at prices the bar may never have traded at. In our
internal testing it inflated one strategy's Sharpe from −2.12 under realistic
fills to +4.00. It is opt-in only. **For deploy decisions,
do not trust idealized numbers without re-validating under at least
`worst_case` for market/stop fills and `print_through_limit` for any
strategy whose entry depends on level-touch.**

The bar-fill price model is **only a coarse proxy** for execution realism on
sparse OHLCV. Validate any fill-sensitive strategy against MBO/L2 (the
`kenoma_execution::MboLimitFillTracker` queue-ahead model is the entry point)
before allocating capital.

## Prerequisites

A stable Rust toolchain (`rustup default stable`). No external market data is
needed: `cargo test --workspace` and the `bar_smoke` quickstart below run
entirely on the checked-in `examples/bar_smoke.csv`. `manifests/bar_smoke.toml`
writes its run artifacts to `target/kenoma-runs/bar-smoke/` relative to the
repo root.

## CLI

```bash
cargo run -p kenoma-cli --bin kenoma-bt -- schema print
cargo run -p kenoma-cli --bin kenoma-bt -- run --manifest manifests/bar_smoke.toml
cargo run -p kenoma-cli --bin kenoma-bt -- audit --run-dir target/kenoma-runs/bar-smoke
```

The library API is the primary strategy interface. The CLI includes built-in
`noop`, `buy_first_bar`, `event_window_long_short`, and `odte_debit_spread`
strategies for smoke testing manifests and first-party bridge bundles.

`odte_debit_spread` is the bridge strategy used by a downstream options project's
exports. It expects a manifest with two option instruments plus `odte_entry` and `odte_exit`
timer events. It buys the long leg, sells the short leg, then flattens the
quantities that actually filled. It relies on quote events and the conservative
causal fill model; it does not simulate broker-native multi-leg spread routing.

## Run Artifacts

V1 writes:

- `manifest.lock.json`
- `audit.json`
- `metrics.json`
- `orders.parquet` and `orders.json`
- `fills.parquet` and `fills.json`
- `positions.parquet` and `positions.json`
- `equity_curve.parquet` and `equity_curve.json`

The canonical schema printer defines the portable bar/event/order/fill schema used by adapters and converters.

## Stress Checks

Run the current invariant suite with:

```bash
cargo test --workspace
cargo check -p kenoma-data --features dbn,parquet
cargo run -q -p kenoma-cli --bin kenoma-bt -- run --manifest manifests/bar_smoke.toml
cargo run -q -p kenoma-cli --bin kenoma-bt -- audit --run-dir target/kenoma-runs/bar-smoke
```

The first stress pass covers same-timestamp fill causality, strict future-feature audit failures, generated quote-stream order/fill causality, conservative off-tick fill rounding, MBO queue-ahead behavior where cancels ahead do not help, and canonical Parquet null preservation.
