# kenoma-backtester

Standalone Rust research backtester for Kenoma pods.

V1 provides:

- A timestamp-ordered event clock for bars, quotes, trades, MBO, timers, orders, fills, and marks.
- Typed `Strategy` API with `StrategyContext` causal feature cutoff auditing.
- Conservative causal fills by default: no same-event market fills, quote and strictly-after-open bar execution, pessimistic bar bracket ambiguity, and MBO queue-ahead limit validation.
- Multi-asset instrument metadata and portfolio accounting for equities, crypto spot/perps, futures, options, FX, fees, borrow, and funding.
- Built-in Black-Scholes-Merton, Black-76, CRR binomial, Greeks, and IV solver.
- DBN MBO ingest support behind the `kenoma-data/dbn` feature.
- Reproducible TOML manifests and structured run artifacts.

## CLI

```bash
cargo run -p kenoma-cli --bin kenoma-bt -- schema print
cargo run -p kenoma-cli --bin kenoma-bt -- run --manifest manifests/bar_smoke.toml
cargo run -p kenoma-cli --bin kenoma-bt -- audit --run-dir target/kenoma-runs/bar-smoke
```

The library API is the primary strategy interface. The CLI includes built-in `noop` and `buy_first_bar` strategies for smoke testing manifests.

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
