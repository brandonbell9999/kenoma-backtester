//! Tests that `on_rollover_boundary` fires on active-contract changes
//! and that the engine force-flats open positions on contract change.

use anyhow::Result;
use kenoma_engine::{
    BacktestEngine, ExecutionConfig, OutputConfig, PortfolioConfig, RolloverResolver, RunManifest,
    RunSection, Strategy, StrategyConfig, StrategyContext, ValidationConfig,
};
use kenoma_types::{
    AssetClass, ContractSpec, FeeSpec, InstrumentSpec, MarketEvent, OrderRequest, OrderSide, Quote,
    TimestampNs,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

fn instrument() -> InstrumentSpec {
    InstrumentSpec {
        id: 1,
        symbol: "ESH26".to_string(),
        asset_class: AssetClass::Future,
        tick_size: 0.25,
        lot_size: 1.0,
        multiplier: 50.0,
        quote_currency: "USD".to_string(),
        base_currency: None,
        session_calendar: None,
        fees: FeeSpec::default(),
        funding: None,
        borrow: None,
        contract: Some(ContractSpec {
            root: Some("ES".to_string()),
            expiry: Some("2026-03-20".to_string()),
            settlement: None,
        }),
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

fn manifest(enable_hg_hooks: bool) -> RunManifest {
    RunManifest {
        run: RunSection {
            id: "rollover_boundary".to_string(),
        },
        data: Vec::new(),
        universe: vec![instrument()],
        portfolio: PortfolioConfig {
            initial_capital: 100_000.0,
            base_currency: "USD".to_string(),
        },
        strategy: StrategyConfig {
            name: "rollover_boundary".to_string(),
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

/// Fixture resolver: returns "ESH26" before t=300, "ESM26" at t>=300.
struct RolloverFixture;

impl RolloverResolver for RolloverFixture {
    fn active_contract(&self, ts_ns: TimestampNs, _instrument_family: &str) -> String {
        if ts_ns < 300 {
            "ESH26".to_string()
        } else {
            "ESM26".to_string()
        }
    }
}

struct RolloverRecorder {
    trace: Rc<RefCell<Vec<String>>>,
    buy_on_first: bool,
    bought: bool,
}

impl Strategy for RolloverRecorder {
    fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
        if self.buy_on_first && !self.bought && matches!(event, MarketEvent::Quote(_)) {
            ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
            self.bought = true;
        }
        Ok(())
    }

    fn on_rollover_boundary(
        &mut self,
        _ctx: &mut StrategyContext,
        instrument_family: &str,
        old_contract: &str,
        new_contract: &str,
    ) -> Result<()> {
        self.trace.borrow_mut().push(format!(
            "rollover:family={}:old={}:new={}",
            instrument_family, old_contract, new_contract
        ));
        Ok(())
    }
}

#[test]
fn on_rollover_boundary_fires_on_contract_change_and_force_flats_open_position() {
    let trace: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let strategy = RolloverRecorder {
        trace: Rc::clone(&trace),
        buy_on_first: true,
        bought: false,
    };

    // Events: quote(100), quote(200), quote(300), quote(400)
    // Strategy buys 1 lot on first quote.
    // Fill happens on second quote (next-event fill).
    // Rollover at t=300: resolver switches from ESH26 to ESM26.
    // Force-flat order submitted at rollover, fills on quote(400).
    let events = vec![
        quote(100, 5000.0, 5000.50),
        quote(200, 5001.0, 5001.50),
        quote(300, 5002.0, 5002.50),
        quote(400, 5003.0, 5003.50),
    ];

    let engine = BacktestEngine::new(strategy, manifest(true))
        .with_rollover_resolver(Box::new(RolloverFixture));
    let mut engine = engine;
    let report = engine.run(events).unwrap();

    // Boundary should fire.
    let trace = trace.borrow();
    assert_eq!(
        *trace,
        vec!["rollover:family=ES:old=ESH26:new=ESM26".to_string()],
        "expected rollover boundary to fire; got: {:?}",
        *trace
    );

    // Should have 2 fills: entry buy + force-flat sell.
    assert_eq!(
        report.fills.len(),
        2,
        "expected 2 fills (entry + force-flat); got: {}",
        report.fills.len()
    );

    // Force-flat fill should carry the ROLLOVER_BOUNDARY tag without
    // overwriting maker/taker liquidity.
    let flat_fill = &report.fills[1];
    assert_eq!(
        flat_fill.tag.as_deref(),
        Some("ROLLOVER_BOUNDARY"),
        "force-flat fill should have tag=ROLLOVER_BOUNDARY; got: {:?}",
        flat_fill.tag
    );
    assert_eq!(
        flat_fill.liquidity.as_deref(),
        Some("taker"),
        "force-flat fill should preserve taker liquidity; got: {:?}",
        flat_fill.liquidity
    );

    // Final position should be flat.
    let position = report
        .positions
        .iter()
        .find(|p| p.instrument_id == 1)
        .expect("position should exist");
    assert!(
        position.qty.abs() < 1e-9,
        "position should be flat after force-flat; qty={}",
        position.qty
    );
}

#[test]
fn on_rollover_boundary_does_not_force_flat_when_already_flat() {
    let trace: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let strategy = RolloverRecorder {
        trace: Rc::clone(&trace),
        buy_on_first: false, // don't open a position
        bought: false,
    };

    let events = vec![
        quote(100, 5000.0, 5000.50),
        quote(200, 5001.0, 5001.50),
        quote(300, 5002.0, 5002.50),
        quote(400, 5003.0, 5003.50),
    ];

    let engine = BacktestEngine::new(strategy, manifest(true))
        .with_rollover_resolver(Box::new(RolloverFixture));
    let mut engine = engine;
    let report = engine.run(events).unwrap();

    // Boundary should still fire.
    let trace = trace.borrow();
    assert_eq!(
        *trace,
        vec!["rollover:family=ES:old=ESH26:new=ESM26".to_string()],
        "expected rollover boundary to fire even with flat position; got: {:?}",
        *trace
    );

    // No fills because no position was opened.
    assert_eq!(
        report.fills.len(),
        0,
        "no fills should occur when strategy doesn't trade; got: {}",
        report.fills.len()
    );
}
