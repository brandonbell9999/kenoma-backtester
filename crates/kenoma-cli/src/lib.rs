//! Library surface for the `kenoma-bt` CLI.
//!
//! Holds reusable engine strategies dispatched by the CLI (currently the
//! generic [`EventWindowLongShort`]) plus an in-memory test harness so
//! integration tests can drive a strategy through the real engine without
//! touching the filesystem.

use anyhow::Result;
use kenoma_backtester::engine::{
    BacktestEngine, ExecutionConfig, MetricsManifestConfig, OutputConfig, PortfolioConfig,
    RunManifest, RunSection, Strategy, StrategyConfig, StrategyContext, ValidationConfig,
};
use kenoma_backtester::execution::BarFillMode;
use kenoma_backtester::types::{
    Fill, InstrumentId, InstrumentSpec, MarketEvent, OrderRequest, OrderSide, RunReport,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Generic scheduled long-short event-window strategy.
///
/// On the first `Bar` per instrument with `ts_open > entry_signal_ts`:
/// - basket members get a market **Buy** of `qty = (long_notional / n_basket) / open`
/// - the ETF gets a market **Sell** of `qty = (hedge_ratio * long_notional) / open`
///
/// On the first bar per instrument with `ts_open >= exit_signal_ts`, the tracked
/// position is flattened with a market order. The engine fills next-bar
/// (conservative causal); under `bar_fill_mode = idealized` that is
/// `open ± half_spread`, which is honest for scheduled market orders.
///
/// There is no engine API to read positions back, so the held quantity per
/// instrument is tracked locally.
#[derive(Debug, Clone)]
pub struct EventWindowLongShort {
    basket_ids: Vec<u32>,
    etf_id: u32,
    hedge_ratio: f64,
    long_notional: f64,
    entry_signal_ts: u64,
    exit_signal_ts: u64,
    qty: BTreeMap<u32, f64>, // signed held qty per instrument
    entered: BTreeMap<u32, bool>,
    exited: BTreeMap<u32, bool>,
}

impl EventWindowLongShort {
    pub fn from_parts(
        basket_ids: Vec<u32>,
        etf_id: u32,
        hedge_ratio: f64,
        long_notional: f64,
        entry_signal_ts: u64,
        exit_signal_ts: u64,
    ) -> Self {
        Self {
            basket_ids,
            etf_id,
            hedge_ratio,
            long_notional,
            entry_signal_ts,
            exit_signal_ts,
            qty: BTreeMap::new(),
            entered: BTreeMap::new(),
            exited: BTreeMap::new(),
        }
    }

    /// Build from manifest `[strategy.params]`.
    ///
    /// Expects: `basket_ids` (comma-separated ints, as a string), `etf_id`
    /// (int), `hedge_ratio` (float), `long_notional` (float), `entry_signal_ts`
    /// (int ns), `exit_signal_ts` (int ns).
    pub fn from_params(p: &BTreeMap<String, toml::Value>) -> Result<Self> {
        let ids = p
            .get("basket_ids")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("event_window_long_short: missing string param 'basket_ids'")
            })?
            .split(',')
            .map(|s| {
                s.trim().parse::<u32>().map_err(|e| {
                    anyhow::anyhow!("event_window_long_short: bad basket id '{s}': {e}")
                })
            })
            .collect::<Result<Vec<u32>>>()?;
        let etf_id = param_int(p, "etf_id")? as u32;
        let hedge_ratio = param_float(p, "hedge_ratio")?;
        let long_notional = param_float(p, "long_notional")?;
        let entry_signal_ts = param_int(p, "entry_signal_ts")? as u64;
        let exit_signal_ts = param_int(p, "exit_signal_ts")? as u64;
        Ok(Self::from_parts(
            ids,
            etf_id,
            hedge_ratio,
            long_notional,
            entry_signal_ts,
            exit_signal_ts,
        ))
    }

    fn is_basket(&self, id: u32) -> bool {
        self.basket_ids.contains(&id)
    }
}

fn param_int(p: &BTreeMap<String, toml::Value>, name: &str) -> Result<i64> {
    p.get(name)
        .and_then(|v| v.as_integer())
        .ok_or_else(|| anyhow::anyhow!("event_window_long_short: missing integer param '{name}'"))
}

fn param_float(p: &BTreeMap<String, toml::Value>, name: &str) -> Result<f64> {
    p.get(name)
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
        .ok_or_else(|| anyhow::anyhow!("event_window_long_short: missing float param '{name}'"))
}

impl Strategy for EventWindowLongShort {
    fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
        let MarketEvent::Bar(bar) = event else {
            return Ok(());
        };
        let id = bar.instrument_id;
        let in_scope = self.is_basket(id) || id == self.etf_id;
        if !in_scope {
            return Ok(());
        }

        // ENTRY: first bar after the signal becomes public.
        if !*self.entered.get(&id).unwrap_or(&false) && bar.ts_open > self.entry_signal_ts {
            let (side, q) = if self.is_basket(id) {
                let n = self.basket_ids.len() as f64;
                (OrderSide::Buy, (self.long_notional / n) / bar.open)
            } else {
                (
                    OrderSide::Sell,
                    (self.hedge_ratio * self.long_notional) / bar.open,
                )
            };
            ctx.record_signal_before_entry("policy_event", self.entry_signal_ts, bar.ts_open)?;
            ctx.submit_order(OrderRequest::market(id, side, q));
            let signed = if side == OrderSide::Buy { q } else { -q };
            self.qty.insert(id, signed);
            self.entered.insert(id, true);
            return Ok(());
        }

        // EXIT: first bar at/after the exit signal.
        if *self.entered.get(&id).unwrap_or(&false)
            && !*self.exited.get(&id).unwrap_or(&false)
            && bar.ts_open >= self.exit_signal_ts
        {
            let held = *self.qty.get(&id).unwrap_or(&0.0);
            if held.abs() > 0.0 {
                let side = if held > 0.0 {
                    OrderSide::Sell
                } else {
                    OrderSide::Buy
                };
                ctx.submit_order(OrderRequest::market(id, side, held.abs()));
            }
            self.exited.insert(id, true);
        }
        Ok(())
    }

    fn on_fill(&mut self, _ctx: &mut StrategyContext, _fill: &Fill) -> Result<()> {
        Ok(())
    }
}

/// Drive a strategy through the real engine over an in-memory event vector.
///
/// Builds a minimal equities manifest covering instrument ids 1, 2, 3 (cash
/// equities via [`InstrumentSpec::default_cash`]) with idealized bar fills, then
/// runs the engine. Intended for integration tests; panics on engine error.
pub fn run_in_memory_for_test(
    strategy: impl Strategy,
    events: Vec<MarketEvent>,
    initial_capital: f64,
) -> RunReport {
    let manifest = minimal_equities_manifest(&[1, 2, 3], initial_capital);
    let mut engine = BacktestEngine::new(strategy, manifest);
    engine.run(events).expect("in-memory backtest run failed")
}

/// Construct a minimal in-memory [`RunManifest`] over the given equity
/// instrument ids with idealized bar fills and no on-disk data sources.
fn minimal_equities_manifest(instrument_ids: &[InstrumentId], initial_capital: f64) -> RunManifest {
    let universe = instrument_ids
        .iter()
        .map(|&id| InstrumentSpec::default_cash(id, format!("SYM{id}"), "USD"))
        .collect::<Vec<_>>();
    RunManifest {
        run: RunSection {
            id: "in-memory-test".to_string(),
        },
        data: Vec::new(),
        universe,
        portfolio: PortfolioConfig {
            initial_capital,
            base_currency: "USD".to_string(),
        },
        strategy: StrategyConfig {
            name: "event_window_long_short".to_string(),
            crate_path: None,
            params: BTreeMap::new(),
        },
        execution: ExecutionConfig {
            policy: "conservative_causal".to_string(),
            bar_fill_mode: BarFillMode::Idealized,
            fixed_spread_ticks: 0.0,
            commission_per_side: 0.0,
            slippage_ticks: 0.0,
            enable_hg_hooks: false,
        },
        validation: ValidationConfig { strict: false },
        metrics: MetricsManifestConfig {
            annualization_factor: None,
        },
        output: OutputConfig {
            dir: PathBuf::from("target/kenoma-runs/in-memory-test"),
        },
    }
}
