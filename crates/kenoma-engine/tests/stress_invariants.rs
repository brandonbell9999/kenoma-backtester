use anyhow::Result;
use kenoma_audit::ValidationMode;
use kenoma_engine::{
    BacktestEngine, ExecutionConfig, OutputConfig, PortfolioConfig, RunManifest, RunSection,
    Strategy, StrategyConfig, StrategyContext, ValidationConfig,
};
use kenoma_types::{
    AssetClass, Bar, FeeSpec, Fill, InstrumentSpec, MarketEvent, OrderRequest, OrderSide, Quote,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn instrument() -> InstrumentSpec {
    InstrumentSpec {
        id: 1,
        symbol: "STRESS".to_string(),
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

fn manifest(strict: bool) -> RunManifest {
    RunManifest {
        run: RunSection {
            id: "stress".to_string(),
        },
        data: Vec::new(),
        universe: vec![instrument()],
        portfolio: PortfolioConfig {
            initial_capital: 10_000.0,
            base_currency: "USD".to_string(),
        },
        strategy: StrategyConfig {
            name: "stress".to_string(),
            crate_path: None,
            params: BTreeMap::new(),
        },
        execution: ExecutionConfig::default(),
        validation: ValidationConfig { strict },
        metrics: kenoma_engine::MetricsManifestConfig::default(),
        output: OutputConfig {
            dir: PathBuf::from("target/stress-test"),
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

fn bar(ts_open: u64, ts_close: u64, open: f64, high: f64, low: f64, close: f64) -> MarketEvent {
    MarketEvent::Bar(Bar {
        instrument_id: 1,
        ts_open,
        ts_close,
        open,
        high,
        low,
        close,
        volume: 1_000.0,
        vwap: None,
        feature_cutoff_ts: Some(ts_close),
    })
}

#[derive(Default)]
struct SubmitOnFirstQuote {
    submitted: bool,
}

impl Strategy for SubmitOnFirstQuote {
    fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
        if !self.submitted && matches!(event, MarketEvent::Quote(_)) {
            ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
            self.submitted = true;
        }
        Ok(())
    }
}

#[test]
fn same_timestamp_bar_after_quote_cannot_fill_new_order() {
    let events = vec![
        quote(100, 99.99, 100.01),
        bar(0, 100, 50.0, 50.0, 50.0, 50.0),
        quote(101, 100.99, 101.01),
    ];
    let mut engine = BacktestEngine::new(SubmitOnFirstQuote::default(), manifest(false));
    let report = engine.run(events).unwrap();
    assert_eq!(report.orders.len(), 1);
    assert_eq!(report.fills.len(), 1);
    assert_eq!(report.fills[0].ts, 101);
    assert_eq!(report.fills[0].price, 101.01);
}

struct LookaheadStrategy;

impl Strategy for LookaheadStrategy {
    fn on_event(&mut self, ctx: &mut StrategyContext, _event: &MarketEvent) -> Result<()> {
        ctx.record_feature_cutoff("future_feature", ctx.now() + 1)
    }
}

#[test]
fn strict_validation_fails_on_future_feature_access() {
    let mut engine = BacktestEngine::new(LookaheadStrategy, manifest(true));
    let err = engine.run(vec![quote(100, 99.99, 100.01)]).unwrap_err();
    assert!(err.to_string().contains("future_feature"));
    assert_eq!(engine.audit().mode, ValidationMode::Strict);
    assert_eq!(engine.audit().warnings().count(), 1);
}

#[derive(Default)]
struct BuyEveryQuote {
    submitted: usize,
    fills: Vec<Fill>,
}

impl Strategy for BuyEveryQuote {
    fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
        if matches!(event, MarketEvent::Quote(_)) && self.submitted < 12 {
            ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
            self.submitted += 1;
        }
        Ok(())
    }

    fn on_fill(&mut self, _ctx: &mut StrategyContext, fill: &Fill) -> Result<()> {
        self.fills.push(fill.clone());
        Ok(())
    }
}

#[test]
fn generated_quote_stream_preserves_order_fill_causality() {
    let events = (0..20)
        .map(|i| quote(100 + i, 99.0 + i as f64 * 0.01, 99.01 + i as f64 * 0.01))
        .collect::<Vec<_>>();
    let mut engine = BacktestEngine::new(BuyEveryQuote::default(), manifest(false));
    let report = engine.run(events).unwrap();
    assert_eq!(report.orders.len(), 12);
    assert_eq!(report.fills.len(), 12);

    let orders_by_id = report
        .orders
        .iter()
        .map(|order| (order.id, order.created_ts))
        .collect::<BTreeMap<_, _>>();
    for fill in &report.fills {
        let created_ts = orders_by_id[&fill.order_id];
        assert!(
            fill.ts > created_ts,
            "fill {} occurred at {} but order was created at {}",
            fill.order_id,
            fill.ts,
            created_ts
        );
    }
}
