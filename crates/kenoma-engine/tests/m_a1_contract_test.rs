//! M-A1 engine contract tests: strategy-driven cancellation, OCO selection,
//! Day TIF expiry, and halt-gated cancellation behavior.

use anyhow::Result;
use kenoma_engine::{
    BacktestEngine, ExecutionConfig, OutputConfig, PortfolioConfig, RunManifest, RunSection,
    SessionResolver, Strategy, StrategyConfig, StrategyContext, ValidationConfig,
};
use kenoma_execution::BarFillMode;
use kenoma_types::{
    AssetClass, Bar, FeeSpec, Fill, InstrumentId, InstrumentSpec, MarketEvent, MboEvent,
    OrderRequest, OrderSide, OrderType, Quote, SessionPhase, TimeInForce, TimerEvent, TimestampNs,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

const IID: InstrumentId = 1;
const DAY_NS: TimestampNs = 86_400_000_000_000;

fn instrument() -> InstrumentSpec {
    InstrumentSpec {
        id: IID,
        symbol: "MA1".to_string(),
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

fn manifest(run_id: &str, enable_hg_hooks: bool) -> RunManifest {
    RunManifest {
        run: RunSection {
            id: run_id.to_string(),
        },
        data: Vec::new(),
        universe: vec![instrument()],
        portfolio: PortfolioConfig {
            initial_capital: 10_000.0,
            base_currency: "USD".to_string(),
        },
        strategy: StrategyConfig {
            name: run_id.to_string(),
            crate_path: None,
            params: BTreeMap::new(),
        },
        execution: ExecutionConfig {
            enable_hg_hooks,
            bar_fill_mode: BarFillMode::WorstCase,
            fixed_spread_ticks: 0.0,
            ..ExecutionConfig::default()
        },
        validation: ValidationConfig { strict: false },
        metrics: Default::default(),
        output: OutputConfig {
            dir: output_dir(),
        },
    }
}

fn quote(ts: TimestampNs, bid: f64, ask: f64) -> MarketEvent {
    MarketEvent::Quote(Quote {
        instrument_id: IID,
        ts,
        bid_price: bid,
        bid_size: 100.0,
        ask_price: ask,
        ask_size: 100.0,
    })
}

fn timer(ts: TimestampNs) -> MarketEvent {
    MarketEvent::Timer(TimerEvent {
        ts,
        name: "tick".to_string(),
    })
}

fn bar(
    ts_open: TimestampNs,
    ts_close: TimestampNs,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
) -> MarketEvent {
    MarketEvent::Bar(Bar {
        instrument_id: IID,
        ts_open,
        ts_close,
        open,
        high,
        low,
        close,
        volume: 100.0,
        vwap: None,
        feature_cutoff_ts: Some(ts_close),
    })
}

fn mbo(
    ts: TimestampNs,
    order_id: u64,
    action: char,
    side: char,
    price: f64,
    size: u32,
) -> MarketEvent {
    MarketEvent::Mbo(MboEvent::from_dbn_parts(
        ts,
        order_id,
        IID,
        action,
        side,
        kenoma_types::price_to_fixed(price),
        size,
        kenoma_book::F_LAST,
    ))
}

fn limit_order(id: u64, side: OrderSide, price: f64) -> OrderRequest {
    let mut order = OrderRequest::limit(IID, side, 1.0, price);
    order.id = id;
    order.tif = TimeInForce::Day;
    order
}

fn market_order(id: u64, side: OrderSide) -> OrderRequest {
    let mut order = OrderRequest::market(IID, side, 1.0);
    order.id = id;
    order.tif = TimeInForce::Day;
    order
}

fn stop_order(id: u64, side: OrderSide, stop_price: f64) -> OrderRequest {
    let mut order = market_order(id, side);
    order.order_type = OrderType::Stop { stop_price };
    order
}

fn with_oco(mut order: OrderRequest, group: &str) -> OrderRequest {
    order.oco_group = Some(group.to_string());
    order
}

#[test]
fn cancel_order_returns_true_and_prevents_later_bar_fill() {
    let outcomes = Rc::new(RefCell::new(Vec::new()));

    struct CancelResting {
        outcomes: Rc<RefCell<Vec<bool>>>,
    }

    impl Strategy for CancelResting {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            match event.timestamp_ns() {
                100 => ctx.submit_order(limit_order(42, OrderSide::Buy, 99.0)),
                101 => {
                    self.outcomes.borrow_mut().push(ctx.cancel_order(42));
                    self.outcomes.borrow_mut().push(ctx.cancel_order(42));
                }
                _ => {}
            }
            Ok(())
        }
    }

    let events = vec![
        quote(100, 100.00, 100.02),
        quote(101, 100.50, 100.52),
        quote(102, 98.50, 98.52),
    ];
    let mut engine = BacktestEngine::new(
        CancelResting {
            outcomes: Rc::clone(&outcomes),
        },
        manifest("ma1_cancel_resting", false),
    );
    let report = engine.run(events).unwrap();

    assert_eq!(*outcomes.borrow(), vec![true, false]);
    assert_eq!(report.orders.len(), 1);
    assert!(
        report.fills.is_empty(),
        "cancelled limit must not fill later"
    );
}

#[test]
fn cancel_unknown_and_already_filled_return_false() {
    let outcomes = Rc::new(RefCell::new(Vec::new()));

    struct UnknownAndFilled {
        outcomes: Rc<RefCell<Vec<bool>>>,
    }

    impl Strategy for UnknownAndFilled {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if event.timestamp_ns() == 100 {
                self.outcomes.borrow_mut().push(ctx.cancel_order(999));
                ctx.submit_order(market_order(43, OrderSide::Buy));
            }
            Ok(())
        }

        fn on_fill(&mut self, ctx: &mut StrategyContext, fill: &Fill) -> Result<()> {
            self.outcomes
                .borrow_mut()
                .push(ctx.cancel_order(fill.order_id));
            Ok(())
        }
    }

    let events = vec![quote(100, 100.00, 100.02), quote(101, 101.00, 101.02)];
    let mut engine = BacktestEngine::new(
        UnknownAndFilled {
            outcomes: Rc::clone(&outcomes),
        },
        manifest("ma1_cancel_false_cases", false),
    );
    let report = engine.run(events).unwrap();

    assert_eq!(*outcomes.borrow(), vec![false, false]);
    assert_eq!(report.fills.len(), 1);
    assert_eq!(report.fills[0].order_id, 43);
}

#[test]
fn cancel_order_removes_mbo_tracker() {
    let outcomes = Rc::new(RefCell::new(Vec::new()));

    struct CancelTrackedMbo {
        outcomes: Rc<RefCell<Vec<bool>>>,
    }

    impl Strategy for CancelTrackedMbo {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(limit_order(44, OrderSide::Buy, 100.0));
            Ok(())
        }

        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if matches!(event, MarketEvent::Mbo(mbo) if mbo.ts == 1) {
                self.outcomes.borrow_mut().push(ctx.cancel_order(44));
            }
            Ok(())
        }
    }

    let events = vec![
        timer(0),
        mbo(1, 10, 'A', 'B', 100.0, 5),
        mbo(2, 11, 'F', 'B', 100.0, 5),
        mbo(3, 12, 'F', 'B', 100.0, 1),
    ];
    let mut engine = BacktestEngine::new(
        CancelTrackedMbo {
            outcomes: Rc::clone(&outcomes),
        },
        manifest("ma1_cancel_mbo_tracker", false),
    );
    let report = engine.run(events).unwrap();

    assert_eq!(*outcomes.borrow(), vec![true]);
    assert!(
        report.fills.is_empty(),
        "cancel must remove both pending order and MBO queue tracker"
    );
}

#[test]
fn oco_target_only_fills_and_cancels_stop() {
    struct TargetOnly;
    impl Strategy for TargetOnly {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(with_oco(
                limit_order(101, OrderSide::Sell, 110.0),
                "long_exit",
            ));
            ctx.submit_order(with_oco(
                stop_order(102, OrderSide::Sell, 95.0),
                "long_exit",
            ));
            Ok(())
        }
    }

    let events = vec![
        timer(0),
        bar(10, 20, 100.0, 111.0, 100.0, 110.0),
        bar(20, 30, 100.0, 101.0, 94.0, 95.0),
    ];
    let mut engine = BacktestEngine::new(TargetOnly, manifest("ma1_oco_target_only", false));
    let report = engine.run(events).unwrap();

    assert_eq!(report.fills.len(), 1);
    assert_eq!(report.fills[0].order_id, 101);
}

#[test]
fn oco_stop_only_fills_and_cancels_target() {
    struct StopOnly;
    impl Strategy for StopOnly {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(with_oco(
                limit_order(201, OrderSide::Sell, 110.0),
                "long_exit",
            ));
            ctx.submit_order(with_oco(
                stop_order(202, OrderSide::Sell, 95.0),
                "long_exit",
            ));
            Ok(())
        }
    }

    let events = vec![
        timer(0),
        bar(10, 20, 100.0, 100.0, 94.0, 95.0),
        bar(20, 30, 100.0, 111.0, 100.0, 110.0),
    ];
    let mut engine = BacktestEngine::new(StopOnly, manifest("ma1_oco_stop_only", false));
    let report = engine.run(events).unwrap();

    assert_eq!(report.fills.len(), 1);
    assert_eq!(report.fills[0].order_id, 202);
}

#[test]
fn same_bar_oco_long_stop_and_target_reachable_fills_stop_only() {
    struct AmbiguousLongExit;
    impl Strategy for AmbiguousLongExit {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(with_oco(
                limit_order(301, OrderSide::Sell, 110.0),
                "long_exit",
            ));
            ctx.submit_order(with_oco(
                stop_order(302, OrderSide::Sell, 95.0),
                "long_exit",
            ));
            Ok(())
        }
    }

    let events = vec![timer(0), bar(10, 20, 100.0, 111.0, 94.0, 100.0)];
    let mut engine =
        BacktestEngine::new(AmbiguousLongExit, manifest("ma1_same_bar_oco_long", false));
    let report = engine.run(events).unwrap();

    assert_eq!(report.fills.len(), 1);
    assert_eq!(report.fills[0].order_id, 302);
    assert_eq!(report.fills[0].price, 94.0);
}

#[test]
fn same_bar_oco_short_stop_and_target_reachable_fills_stop_only() {
    struct AmbiguousShortExit;
    impl Strategy for AmbiguousShortExit {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(with_oco(
                limit_order(401, OrderSide::Buy, 90.0),
                "short_exit",
            ));
            ctx.submit_order(with_oco(
                stop_order(402, OrderSide::Buy, 105.0),
                "short_exit",
            ));
            Ok(())
        }
    }

    let events = vec![timer(0), bar(10, 20, 100.0, 106.0, 89.0, 100.0)];
    let mut engine = BacktestEngine::new(
        AmbiguousShortExit,
        manifest("ma1_same_bar_oco_short", false),
    );
    let report = engine.run(events).unwrap();

    assert_eq!(report.fills.len(), 1);
    assert_eq!(report.fills[0].order_id, 402);
    assert_eq!(report.fills[0].price, 106.0);
}

#[test]
fn non_oco_orders_both_fill_when_reachable() {
    struct IndependentOrders;
    impl Strategy for IndependentOrders {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(limit_order(501, OrderSide::Sell, 110.0));
            ctx.submit_order(stop_order(502, OrderSide::Sell, 95.0));
            Ok(())
        }
    }

    let events = vec![timer(0), bar(10, 20, 100.0, 111.0, 94.0, 100.0)];
    let mut engine = BacktestEngine::new(IndependentOrders, manifest("ma1_non_oco", false));
    let report = engine.run(events).unwrap();
    let filled_ids = report
        .fills
        .iter()
        .map(|fill| fill.order_id)
        .collect::<BTreeSet<_>>();

    assert_eq!(report.fills.len(), 2);
    assert_eq!(filled_ids, BTreeSet::from([501, 502]));
}

#[test]
fn oco_sibling_cancel_prevents_double_fill_before_on_fill_followups() {
    struct FollowupAfterStop;
    impl Strategy for FollowupAfterStop {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(with_oco(
                limit_order(601, OrderSide::Sell, 110.0),
                "long_exit",
            ));
            ctx.submit_order(with_oco(
                stop_order(602, OrderSide::Sell, 95.0),
                "long_exit",
            ));
            Ok(())
        }

        fn on_fill(&mut self, ctx: &mut StrategyContext, fill: &Fill) -> Result<()> {
            if fill.order_id == 602 {
                ctx.submit_order(market_order(603, OrderSide::Buy));
            }
            Ok(())
        }
    }

    let events = vec![
        timer(0),
        bar(10, 20, 100.0, 111.0, 94.0, 100.0),
        quote(21, 100.00, 100.02),
    ];
    let mut engine = BacktestEngine::new(
        FollowupAfterStop,
        manifest("ma1_oco_before_followups", false),
    );
    let report = engine.run(events).unwrap();
    let fill_ids = report
        .fills
        .iter()
        .map(|fill| fill.order_id)
        .collect::<Vec<_>>();

    assert_eq!(fill_ids, vec![602, 603]);
}

#[test]
fn day_tif_order_expires_before_next_trading_date_fill() {
    struct SubmitDayLimit;
    impl Strategy for SubmitDayLimit {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(limit_order(701, OrderSide::Buy, 99.0));
            Ok(())
        }
    }

    let events = vec![
        timer(1),
        quote(100, 100.00, 100.02),
        quote(DAY_NS + 100, 98.50, 98.52),
    ];
    let mut engine =
        BacktestEngine::new(SubmitDayLimit, manifest("ma1_day_tif_date_boundary", false));
    let report = engine.run(events).unwrap();

    assert!(
        report.fills.is_empty(),
        "Day resting order must expire before it can fill on the next trading date"
    );
}

struct HaltAt200;
impl SessionResolver for HaltAt200 {
    fn session_phase(&self, ts_ns: TimestampNs, _instrument_id: InstrumentId) -> SessionPhase {
        if ts_ns < 200 {
            SessionPhase::Rth
        } else {
            SessionPhase::Halt
        }
    }
}

#[test]
fn halt_cancels_resting_day_orders_only_when_hg_hooks_enabled() {
    struct SubmitRestingDay;
    impl Strategy for SubmitRestingDay {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(limit_order(801, OrderSide::Buy, 99.0));
            Ok(())
        }
    }

    let events = vec![
        timer(0),
        quote(100, 100.00, 100.02),
        quote(200, 100.00, 100.02),
        quote(300, 98.50, 98.52),
    ];

    let mut disabled =
        BacktestEngine::new(SubmitRestingDay, manifest("ma1_halt_disabled_gate", false))
            .with_session_resolver(Box::new(HaltAt200));
    let disabled_report = disabled.run(events.clone()).unwrap();
    assert_eq!(
        disabled_report.fills.len(),
        1,
        "with hg hooks disabled, Halt resolver behavior must be inert"
    );

    let mut enabled =
        BacktestEngine::new(SubmitRestingDay, manifest("ma1_halt_enabled_cancel", true))
            .with_session_resolver(Box::new(HaltAt200));
    let enabled_report = enabled.run(events).unwrap();
    assert!(
        enabled_report.fills.is_empty(),
        "with hg hooks enabled, Halt should cancel pre-existing resting Day orders"
    );
}

struct AlwaysHalt;
impl SessionResolver for AlwaysHalt {
    fn session_phase(&self, _ts_ns: TimestampNs, _instrument_id: InstrumentId) -> SessionPhase {
        SessionPhase::Halt
    }
}

#[test]
fn market_only_halt_fixture_has_zero_cancels_and_identical_legacy_behavior() {
    #[derive(Default)]
    struct MarketOnly {
        submitted: bool,
    }

    impl Strategy for MarketOnly {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if !self.submitted && matches!(event, MarketEvent::Quote(_)) {
                ctx.submit_order(market_order(901, OrderSide::Buy));
                self.submitted = true;
            }
            Ok(())
        }
    }

    let events = vec![
        quote(100, 100.00, 100.02),
        quote(200, 101.00, 101.02),
        quote(300, 102.00, 102.02),
    ];

    let mut legacy = BacktestEngine::new(
        MarketOnly::default(),
        manifest("ma1_market_only_halt_legacy", false),
    )
    .with_session_resolver(Box::new(AlwaysHalt));
    let legacy_report = legacy.run(events.clone()).unwrap();

    let mut hooks_enabled = BacktestEngine::new(
        MarketOnly::default(),
        manifest("ma1_market_only_halt_hooks", true),
    )
    .with_session_resolver(Box::new(AlwaysHalt));
    let hooks_report = hooks_enabled.run(events).unwrap();

    assert_eq!(legacy_report.orders, hooks_report.orders);
    assert_eq!(legacy_report.fills, hooks_report.fills);
    assert_eq!(legacy_report.positions, hooks_report.positions);
    assert_eq!(legacy_report.equity_curve, hooks_report.equity_curve);
    assert_eq!(
        hooks_report.fills.len(),
        1,
        "market-only Halt path should have zero resting-order cancels and keep the legacy fill"
    );
}

#[test]
fn current_event_fillable_stop_beats_same_bar_cancel_attempt() {
    let outcomes = Rc::new(RefCell::new(Vec::new()));

    struct CancelAfterStopReachable {
        outcomes: Rc<RefCell<Vec<bool>>>,
    }

    impl Strategy for CancelAfterStopReachable {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            ctx.submit_order(stop_order(1001, OrderSide::Sell, 95.0));
            Ok(())
        }

        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if event.timestamp_ns() == 20 {
                self.outcomes.borrow_mut().push(ctx.cancel_order(1001));
            }
            Ok(())
        }
    }

    let events = vec![timer(0), bar(10, 20, 100.0, 100.0, 94.0, 95.0)];
    let mut engine = BacktestEngine::new(
        CancelAfterStopReachable {
            outcomes: Rc::clone(&outcomes),
        },
        manifest("ma1_cancel_loses_to_current_fill", false),
    );
    let report = engine.run(events).unwrap();

    assert_eq!(*outcomes.borrow(), vec![false]);
    assert_eq!(report.fills.len(), 1);
    assert_eq!(report.fills[0].order_id, 1001);
}
