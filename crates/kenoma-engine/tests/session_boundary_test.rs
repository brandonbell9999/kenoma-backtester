//! Tests that `on_session_boundary` fires on session-phase transitions
//! when `ExecutionConfig.enable_hg_hooks == true`.

use anyhow::Result;
use kenoma_engine::{
    BacktestEngine, ExecutionConfig, OutputConfig, PortfolioConfig, RunManifest, RunSection,
    SessionResolver, Strategy, StrategyConfig, StrategyContext, ValidationConfig,
};
use kenoma_types::{
    AssetClass, FeeSpec, InstrumentId, InstrumentSpec, MarketEvent, Quote, SessionPhase,
    TimestampNs,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

fn instrument(id: InstrumentId, sym: &str) -> InstrumentSpec {
    InstrumentSpec {
        id,
        symbol: sym.to_string(),
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

fn manifest(enable_hg_hooks: bool) -> RunManifest {
    RunManifest {
        run: RunSection {
            id: "session_boundary".to_string(),
        },
        data: Vec::new(),
        universe: vec![instrument(1, "TEST")],
        portfolio: PortfolioConfig {
            initial_capital: 10_000.0,
            base_currency: "USD".to_string(),
        },
        strategy: StrategyConfig {
            name: "session_boundary".to_string(),
            crate_path: None,
            params: BTreeMap::new(),
        },
        execution: ExecutionConfig {
            enable_hg_hooks,
            ..ExecutionConfig::default()
        },
        validation: ValidationConfig { strict: false },
        metrics: Default::default(),
        output: OutputConfig {
            dir: std::path::PathBuf::from("/tmp/session_boundary_test"),
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

/// Fixture resolver: Eth before t=200, Rth in [200,400), Halt at >=400.
struct FixtureResolver;

impl SessionResolver for FixtureResolver {
    fn session_phase(&self, ts_ns: TimestampNs, _instrument_id: InstrumentId) -> SessionPhase {
        if ts_ns < 200 {
            SessionPhase::Eth
        } else if ts_ns < 400 {
            SessionPhase::Rth
        } else {
            SessionPhase::Halt
        }
    }
}

struct BoundaryRecorder {
    trace: Rc<RefCell<Vec<String>>>,
}

impl Strategy for BoundaryRecorder {
    fn on_session_boundary(
        &mut self,
        _ctx: &mut StrategyContext,
        _instrument_id: InstrumentId,
        new_phase: SessionPhase,
    ) -> Result<()> {
        self.trace.borrow_mut().push(format!("{:?}", new_phase));
        Ok(())
    }
}

#[test]
fn on_session_boundary_fires_on_each_phase_transition() {
    let trace: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let strategy = BoundaryRecorder {
        trace: Rc::clone(&trace),
    };

    // 6 quotes: 100, 150 (both Eth), 200, 300 (both Rth), 400, 500 (both Halt).
    let events = vec![
        quote(100, 99.0, 100.0),
        quote(150, 99.0, 100.0),
        quote(200, 99.0, 100.0),
        quote(300, 99.0, 100.0),
        quote(400, 99.0, 100.0),
        quote(500, 99.0, 100.0),
    ];

    let engine = BacktestEngine::new(strategy, manifest(true))
        .with_session_resolver(Box::new(FixtureResolver));
    let mut engine = engine;
    engine.run(events).unwrap();

    let trace = trace.borrow();
    assert_eq!(
        *trace,
        vec!["Eth".to_string(), "Rth".to_string(), "Halt".to_string()],
        "expected 3 boundaries: None->Eth, Eth->Rth, Rth->Halt; got: {:?}",
        *trace
    );
}

#[test]
fn on_session_boundary_does_not_fire_when_hooks_disabled() {
    let trace: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let strategy = BoundaryRecorder {
        trace: Rc::clone(&trace),
    };

    let events = vec![
        quote(100, 99.0, 100.0),
        quote(200, 99.0, 100.0),
        quote(400, 99.0, 100.0),
    ];

    let engine = BacktestEngine::new(strategy, manifest(false))
        .with_session_resolver(Box::new(FixtureResolver));
    let mut engine = engine;
    engine.run(events).unwrap();

    let trace = trace.borrow();
    assert!(
        trace.is_empty(),
        "with enable_hg_hooks=false, no boundaries should fire; got: {:?}",
        *trace
    );
}
