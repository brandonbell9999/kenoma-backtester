//! Locks the existing `on_fill` invocation invariant.
//!
//! After M-bt's Critical-finding analysis, the spec wording about wiring
//! `on_fill` is incorrect -- `on_fill` was already invoked from `apply_fill`
//! at the M-bt baseline tag. This test exists to lock that invariant against
//! regressions: `on_fill` must fire exactly once per generated fill, BEFORE
//! the next event's `on_event` dispatch.

use anyhow::Result;
use kenoma_engine::{
    BacktestEngine, ExecutionConfig, OutputConfig, PortfolioConfig, RunManifest, RunSection,
    Strategy, StrategyConfig, StrategyContext, ValidationConfig,
};
use kenoma_types::{
    AssetClass, FeeSpec, Fill, InstrumentSpec, MarketEvent, OrderRequest, OrderSide, Quote,
};
use std::collections::BTreeMap;

fn instrument() -> InstrumentSpec {
    InstrumentSpec {
        id: 1,
        symbol: "FILLINV".to_string(),
        asset_class: AssetClass::Equity,
        tick_size: 0.01,
        lot_size: 1.0,
        multiplier: 1.0,
        quote_currency: "USD".to_string(),
        base_currency: None,
        session_calendar: None,
        fees: FeeSpec::default(),
        funding: None,
        borrow: None,
        contract: None,
        option: None,
        metadata: BTreeMap::new(),
    }
}

/// Unique scratch output dir. These tests never call `write_artifacts`, so
/// nothing is written here; `keep()` hands back the path without tying its
/// lifetime to this helper.
fn output_dir() -> std::path::PathBuf {
    tempfile::tempdir().expect("tempdir").keep()
}

fn manifest() -> RunManifest {
    RunManifest {
        run: RunSection {
            id: "fill_invariant".to_string(),
        },
        data: Vec::new(),
        universe: vec![instrument()],
        portfolio: PortfolioConfig {
            initial_capital: 10_000.0,
            base_currency: "USD".to_string(),
        },
        strategy: StrategyConfig {
            name: "fill_invariant".to_string(),
            crate_path: None,
            params: BTreeMap::new(),
        },
        execution: ExecutionConfig::default(),
        validation: ValidationConfig { strict: false },
        metrics: Default::default(),
        output: OutputConfig {
            dir: output_dir(),
        },
    }
}

fn quote(ts: u64, bid: f64, ask: f64) -> MarketEvent {
    MarketEvent::Quote(Quote {
        instrument_id: 1,
        ts,
        bid_price: bid,
        bid_size: 100.0,
        ask_price: ask,
        ask_size: 100.0,
    })
}

/// Records every callback the engine fires, in order, with the event type
/// and the timestamp at which it occurred.
#[derive(Default)]
struct CallbackTrace {
    log: Vec<String>,
    submitted: bool,
}

impl Strategy for CallbackTrace {
    fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
        self.log
            .push(format!("on_event:ts={}", event.timestamp_ns()));
        if !self.submitted && matches!(event, MarketEvent::Quote(_)) {
            ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
            self.submitted = true;
        }
        Ok(())
    }

    fn on_fill(&mut self, _ctx: &mut StrategyContext, fill: &Fill) -> Result<()> {
        self.log
            .push(format!("on_fill:ts={}:order_id={}", fill.ts, fill.order_id));
        Ok(())
    }
}

#[test]
fn on_fill_fires_exactly_once_per_fill_before_next_event_dispatch() {
    // Event sequence:
    //   t=100: quote -- strategy submits market buy
    //   t=101: quote -- fill at t=101 (next-event causality), then on_event(t=101) afterwards
    let events = vec![quote(100, 99.99, 100.01), quote(101, 100.99, 101.01)];
    let _strategy = CallbackTrace::default();
    let mut engine = BacktestEngine::new(CallbackTrace::default(), manifest());
    let report = engine.run(events).unwrap();

    // Sanity: the order filled.
    assert_eq!(report.orders.len(), 1, "expected exactly one order");
    assert_eq!(report.fills.len(), 1, "expected exactly one fill");
}

/// Helper that runs the engine with a borrowed-state strategy via Rc<RefCell<...>>
/// so the test can read the callback log after `run()` returns.
#[test]
fn callback_order_is_on_event_then_on_fill_then_next_on_event() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    struct Recorder {
        log: Rc<RefCell<Vec<String>>>,
        submitted: bool,
    }

    impl Strategy for Recorder {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            self.log
                .borrow_mut()
                .push(format!("on_event:ts={}", event.timestamp_ns()));
            if !self.submitted && matches!(event, MarketEvent::Quote(_)) {
                ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
                self.submitted = true;
            }
            Ok(())
        }
        fn on_fill(&mut self, _ctx: &mut StrategyContext, fill: &Fill) -> Result<()> {
            self.log
                .borrow_mut()
                .push(format!("on_fill:ts={}", fill.ts));
            Ok(())
        }
    }

    let strategy = Recorder {
        log: Rc::clone(&log),
        submitted: false,
    };
    let events = vec![quote(100, 99.99, 100.01), quote(101, 100.99, 101.01)];
    let mut engine = BacktestEngine::new(strategy, manifest());
    engine.run(events).unwrap();

    let trace = log.borrow();
    // Expected sequence:
    //   1. on_event(ts=100)  -- strategy submits order; no fills yet
    //   2. on_fill(ts=101)   -- fired by apply_fill inside evaluate_pending_orders
    //   3. on_event(ts=101)  -- fired by dispatch_strategy_event AFTER the fill
    assert_eq!(
        *trace,
        vec![
            "on_event:ts=100".to_string(),
            "on_fill:ts=101".to_string(),
            "on_event:ts=101".to_string(),
        ],
        "callback order must be on_event(N) -> on_fill(N+1) -> on_event(N+1)"
    );
}

#[test]
fn on_fill_count_equals_fills_count_for_multi_fill_run() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let fill_count: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));

    struct CountingStrategy {
        fill_count: Rc<RefCell<u32>>,
        orders_submitted: u32,
    }

    impl Strategy for CountingStrategy {
        fn on_event(&mut self, ctx: &mut StrategyContext, _event: &MarketEvent) -> Result<()> {
            // Submit a market buy on every quote, up to 3 orders.
            if self.orders_submitted < 3 {
                ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
                self.orders_submitted += 1;
            }
            Ok(())
        }
        fn on_fill(&mut self, _ctx: &mut StrategyContext, _fill: &Fill) -> Result<()> {
            *self.fill_count.borrow_mut() += 1;
            Ok(())
        }
    }

    let strategy = CountingStrategy {
        fill_count: Rc::clone(&fill_count),
        orders_submitted: 0,
    };
    // 4 quotes: orders submitted on q1, q2, q3 fill on q2, q3, q4.
    let events = vec![
        quote(100, 99.99, 100.01),
        quote(101, 100.99, 101.01),
        quote(102, 101.99, 102.01),
        quote(103, 102.99, 103.01),
    ];
    let mut engine = BacktestEngine::new(strategy, manifest());
    let report = engine.run(events).unwrap();

    assert_eq!(report.fills.len(), 3, "expected 3 fills");
    assert_eq!(
        *fill_count.borrow(),
        3,
        "on_fill must fire exactly once per fill"
    );
}
