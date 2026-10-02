//! Backwards-compat regression test: with `enable_hg_hooks=false` (default),
//! M-bt's additions are inert. No boundary hooks fire, no resolvers queried,
//! no force-flat fills emitted, identical run output to baseline.

use anyhow::Result;
use kenoma_engine::{
    BacktestEngine, ExecutionConfig, OutputConfig, PortfolioConfig, RolloverResolver, RunManifest,
    RunSection, SessionResolver, Strategy, StrategyConfig, StrategyContext, ValidationConfig,
};
use kenoma_types::{
    AssetClass, ContractSpec, FeeSpec, InstrumentId, InstrumentSpec, MarketEvent, OrderRequest,
    OrderSide, Quote, SessionPhase, TimestampNs,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

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
            expiry: Some("2026-03-20".to_string()),
            root: Some("ES".to_string()),
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

fn manifest_with_default_execution() -> RunManifest {
    RunManifest {
        run: RunSection {
            id: "compat".to_string(),
        },
        data: Vec::new(),
        universe: vec![instrument()],
        portfolio: PortfolioConfig {
            initial_capital: 100_000.0,
            base_currency: "USD".to_string(),
        },
        strategy: StrategyConfig {
            name: "compat".to_string(),
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

struct CountingSessionResolver {
    count: Arc<AtomicU32>,
}

impl SessionResolver for CountingSessionResolver {
    fn session_phase(&self, _ts_ns: TimestampNs, _instrument_id: InstrumentId) -> SessionPhase {
        self.count.fetch_add(1, Ordering::Relaxed);
        SessionPhase::Eth
    }
}

struct CountingRolloverResolver {
    count: Arc<AtomicU32>,
}

impl RolloverResolver for CountingRolloverResolver {
    fn active_contract(&self, ts_ns: TimestampNs, _family: &str) -> String {
        self.count.fetch_add(1, Ordering::Relaxed);
        if ts_ns < 200 {
            "ESH26".to_string()
        } else {
            "ESM26".to_string()
        }
    }
}

#[test]
fn resolvers_are_not_queried_when_enable_hg_hooks_is_false() {
    let session_count = Arc::new(AtomicU32::new(0));
    let rollover_count = Arc::new(AtomicU32::new(0));

    struct AssertNoBoundary;
    impl Strategy for AssertNoBoundary {
        fn on_session_boundary(
            &mut self,
            _ctx: &mut StrategyContext,
            _iid: InstrumentId,
            _phase: SessionPhase,
        ) -> Result<()> {
            panic!("on_session_boundary fired with enable_hg_hooks=false (regression!)");
        }
        fn on_rollover_boundary(
            &mut self,
            _ctx: &mut StrategyContext,
            _family: &str,
            _old: &str,
            _new: &str,
        ) -> Result<()> {
            panic!("on_rollover_boundary fired with enable_hg_hooks=false (regression!)");
        }
    }

    let events = vec![
        quote(100, 99.99, 100.01),
        quote(200, 99.99, 100.01),
        quote(300, 99.99, 100.01),
    ];

    let mut engine = BacktestEngine::new(AssertNoBoundary, manifest_with_default_execution())
        .with_session_resolver(Box::new(CountingSessionResolver {
            count: Arc::clone(&session_count),
        }))
        .with_rollover_resolver(Box::new(CountingRolloverResolver {
            count: Arc::clone(&rollover_count),
        }));
    engine.run(events).unwrap();

    assert_eq!(
        session_count.load(Ordering::Relaxed),
        0,
        "SessionResolver must NOT be queried when enable_hg_hooks=false"
    );
    assert_eq!(
        rollover_count.load(Ordering::Relaxed),
        0,
        "RolloverResolver must NOT be queried when enable_hg_hooks=false"
    );
}

#[test]
fn baseline_strategy_produces_baseline_output_with_enable_hg_hooks_false() {
    struct BuyOnce {
        submitted: bool,
    }
    impl Strategy for BuyOnce {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if !self.submitted && matches!(event, MarketEvent::Quote(_)) {
                ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
                self.submitted = true;
            }
            Ok(())
        }
    }
    let events = vec![
        quote(100, 99.99, 100.01),
        quote(200, 99.99, 100.01),
        quote(300, 99.99, 100.01),
    ];
    let mut engine = BacktestEngine::new(
        BuyOnce { submitted: false },
        manifest_with_default_execution(),
    );
    let report = engine.run(events).unwrap();

    assert_eq!(report.orders.len(), 1, "exactly one order (entry)");
    assert_eq!(
        report.fills.len(),
        1,
        "exactly one fill (no force-flat in disabled mode)"
    );
    assert_eq!(report.fills[0].side, OrderSide::Buy);
}
