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

/// Timer-driven 0DTE debit-spread strategy for bundles emitted by
/// downstream consumers.
///
/// The strategy expects quote events for two option instruments:
/// - `odte_entry` timer: buy the long leg and sell the short leg.
/// - `odte_exit` timer: flatten the quantities that actually filled.
///
/// Market orders are still filled by the engine's conservative causal model on
/// subsequent quotes; the timers only create the orders.
#[derive(Debug, Clone)]
pub struct OdteDebitSpread {
    long_leg_id: u32,
    short_leg_id: u32,
    contracts: f64,
    entry_signal_ts: u64,
    exit_signal_ts: u64,
    entered: bool,
    exit_submitted: bool,
    held_qty: BTreeMap<u32, f64>,
}

impl OdteDebitSpread {
    pub fn from_parts(
        long_leg_id: u32,
        short_leg_id: u32,
        contracts: f64,
        entry_signal_ts: u64,
        exit_signal_ts: u64,
    ) -> Result<Self> {
        if long_leg_id == short_leg_id {
            anyhow::bail!("odte_debit_spread: long_leg_id and short_leg_id must differ");
        }
        if !contracts.is_finite() || contracts <= 0.0 {
            anyhow::bail!("odte_debit_spread: contracts must be finite and > 0");
        }
        if exit_signal_ts <= entry_signal_ts {
            anyhow::bail!("odte_debit_spread: exit_signal_ts must be after entry_signal_ts");
        }
        Ok(Self {
            long_leg_id,
            short_leg_id,
            contracts,
            entry_signal_ts,
            exit_signal_ts,
            entered: false,
            exit_submitted: false,
            held_qty: BTreeMap::new(),
        })
    }

    /// Build from manifest `[strategy.params]`.
    ///
    /// Expects: `long_leg_id` (int), `short_leg_id` (int), `contracts`
    /// (float), `entry_signal_ts` (int ns), and `exit_signal_ts` (int ns).
    pub fn from_params(p: &BTreeMap<String, toml::Value>) -> Result<Self> {
        let long_leg_id = odte_param_int(p, "long_leg_id")? as u32;
        let short_leg_id = odte_param_int(p, "short_leg_id")? as u32;
        let contracts = odte_param_float(p, "contracts")?;
        let entry_signal_ts = odte_param_int(p, "entry_signal_ts")? as u64;
        let exit_signal_ts = odte_param_int(p, "exit_signal_ts")? as u64;
        Self::from_parts(
            long_leg_id,
            short_leg_id,
            contracts,
            entry_signal_ts,
            exit_signal_ts,
        )
    }

    fn submit_entry(&mut self, ctx: &mut StrategyContext) {
        let mut long_order = OrderRequest::market(self.long_leg_id, OrderSide::Buy, self.contracts);
        long_order.tag = Some("odte_entry_long".to_string());
        ctx.submit_order(long_order);

        let mut short_order =
            OrderRequest::market(self.short_leg_id, OrderSide::Sell, self.contracts);
        short_order.tag = Some("odte_entry_short".to_string());
        ctx.submit_order(short_order);
        self.entered = true;
    }

    fn submit_exit(&mut self, ctx: &mut StrategyContext) {
        for instrument_id in [self.long_leg_id, self.short_leg_id] {
            let qty = *self.held_qty.get(&instrument_id).unwrap_or(&0.0);
            if qty.abs() <= f64::EPSILON {
                continue;
            }
            let side = if qty > 0.0 {
                OrderSide::Sell
            } else {
                OrderSide::Buy
            };
            let mut order = OrderRequest::market(instrument_id, side, qty.abs());
            order.tag = Some("odte_exit".to_string());
            ctx.submit_order(order);
        }
        self.exit_submitted = true;
    }
}

fn odte_param_int(p: &BTreeMap<String, toml::Value>, name: &str) -> Result<i64> {
    p.get(name)
        .and_then(|v| v.as_integer())
        .ok_or_else(|| anyhow::anyhow!("odte_debit_spread: missing integer param '{name}'"))
}

fn odte_param_float(p: &BTreeMap<String, toml::Value>, name: &str) -> Result<f64> {
    p.get(name)
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
        .ok_or_else(|| anyhow::anyhow!("odte_debit_spread: missing float param '{name}'"))
}

impl Strategy for OdteDebitSpread {
    fn on_timer(&mut self, ctx: &mut StrategyContext, name: &str) -> Result<()> {
        if name == "odte_entry" && !self.entered && ctx.now() >= self.entry_signal_ts {
            self.submit_entry(ctx);
        }
        if name == "odte_exit"
            && self.entered
            && !self.exit_submitted
            && ctx.now() >= self.exit_signal_ts
        {
            self.submit_exit(ctx);
        }
        Ok(())
    }

    fn on_fill(&mut self, _ctx: &mut StrategyContext, fill: &Fill) -> Result<()> {
        if fill.instrument_id == self.long_leg_id || fill.instrument_id == self.short_leg_id {
            *self.held_qty.entry(fill.instrument_id).or_default() += fill.qty * fill.side.sign();
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use kenoma_backtester::types::{
        AssetClass, ExerciseStyle, FeeSpec, OptionContract, OptionKind, Quote, TimerEvent,
    };

    const LONG_ID: u32 = 101;
    const SHORT_ID: u32 = 102;

    #[test]
    fn odte_debit_spread_opens_and_flattens_on_timer_driven_quotes() {
        let strategy = OdteDebitSpread::from_parts(LONG_ID, SHORT_ID, 1.0, 100, 200).unwrap();
        let manifest = options_manifest();
        let mut engine = BacktestEngine::new(strategy, manifest);
        let report = engine
            .run(vec![
                timer(100, "odte_entry"),
                quote(LONG_ID, 101, 2.00, 2.10),
                quote(SHORT_ID, 101, 0.90, 1.00),
                timer(200, "odte_exit"),
                quote(LONG_ID, 201, 3.00, 3.10),
                quote(SHORT_ID, 201, 1.10, 1.20),
            ])
            .unwrap();

        assert_eq!(report.fills.len(), 4);
        assert_eq!(report.fills[0].instrument_id, LONG_ID);
        assert_eq!(report.fills[0].side, OrderSide::Buy);
        assert_eq!(report.fills[0].price, 2.10);
        assert_eq!(report.fills[1].instrument_id, SHORT_ID);
        assert_eq!(report.fills[1].side, OrderSide::Sell);
        assert_eq!(report.fills[1].price, 0.90);
        assert_eq!(report.fills[2].instrument_id, LONG_ID);
        assert_eq!(report.fills[2].side, OrderSide::Sell);
        assert_eq!(report.fills[2].price, 3.00);
        assert_eq!(report.fills[3].instrument_id, SHORT_ID);
        assert_eq!(report.fills[3].side, OrderSide::Buy);
        assert_eq!(report.fills[3].price, 1.20);
        assert!(report
            .positions
            .iter()
            .all(|position| position.qty.abs() < 1e-9));
        assert!((report.metrics.end_equity - 10_060.0).abs() < 1e-9);
    }

    fn options_manifest() -> RunManifest {
        RunManifest {
            run: RunSection {
                id: "odte-test".to_string(),
            },
            data: Vec::new(),
            universe: vec![
                InstrumentSpec::default_cash(1, "SPY", "USD"),
                option_spec(LONG_ID, "SPY_20251201_C_100", 100.0),
                option_spec(SHORT_ID, "SPY_20251201_C_103", 103.0),
            ],
            portfolio: PortfolioConfig {
                initial_capital: 10_000.0,
                base_currency: "USD".to_string(),
            },
            strategy: StrategyConfig {
                name: "odte_debit_spread".to_string(),
                crate_path: None,
                params: BTreeMap::new(),
            },
            execution: ExecutionConfig {
                policy: "conservative_causal".to_string(),
                bar_fill_mode: BarFillMode::WorstCase,
                fixed_spread_ticks: 0.0,
                commission_per_side: 0.0,
                slippage_ticks: 0.0,
                enable_hg_hooks: false,
            },
            validation: ValidationConfig { strict: true },
            metrics: MetricsManifestConfig {
                annualization_factor: None,
            },
            output: OutputConfig {
                dir: PathBuf::from("target/kenoma-runs/odte-test"),
            },
        }
    }

    fn option_spec(id: u32, symbol: &str, strike: f64) -> InstrumentSpec {
        InstrumentSpec {
            id,
            symbol: symbol.to_string(),
            asset_class: AssetClass::Option,
            tick_size: 0.01,
            lot_size: 1.0,
            multiplier: 100.0,
            quote_currency: "USD".to_string(),
            base_currency: None,
            session_calendar: Some("us_equity_options".to_string()),
            fees: FeeSpec::default(),
            funding: None,
            borrow: None,
            contract: None,
            option: Some(OptionContract {
                underlying_id: 1,
                strike,
                expiry: "2025-12-01".to_string(),
                kind: OptionKind::Call,
                exercise: ExerciseStyle::American,
            }),
            metadata: BTreeMap::new(),
        }
    }

    fn timer(ts: u64, name: &str) -> MarketEvent {
        MarketEvent::Timer(TimerEvent {
            ts,
            name: name.to_string(),
        })
    }

    fn quote(instrument_id: u32, ts: u64, bid_price: f64, ask_price: f64) -> MarketEvent {
        MarketEvent::Quote(Quote {
            instrument_id,
            ts,
            bid_price,
            bid_size: 10.0,
            ask_price,
            ask_size: 10.0,
        })
    }
}
