//! Timestamp-ordered backtest engine, strategy API, manifests, and artifacts.

use anyhow::{Context as AnyhowContext, Result};
use kenoma_audit::{AuditSeverity, AuditTrail, ValidationMode};
use kenoma_data::{load_sources, DataSourceConfig};
use kenoma_data::{
    write_equity_curve_parquet, write_fills_parquet, write_orders_parquet, write_positions_parquet,
};
use kenoma_execution::ConservativeCausalFillModel;
use kenoma_portfolio::Portfolio;
use kenoma_stats::{compute_metrics, EquityPoint as StatsEquityPoint, TradePnl};
use kenoma_types::{
    Bar, EquityPoint, Fill, InstrumentId, InstrumentSpec, MarketEvent, OrderRequest, Price, Quote,
    RunMetrics, RunReport, TimestampNs,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

pub trait Strategy {
    fn on_event(&mut self, _ctx: &mut StrategyContext, _event: &MarketEvent) -> Result<()> {
        Ok(())
    }

    fn on_timer(&mut self, _ctx: &mut StrategyContext, _name: &str) -> Result<()> {
        Ok(())
    }

    fn on_fill(&mut self, _ctx: &mut StrategyContext, _fill: &Fill) -> Result<()> {
        Ok(())
    }

    fn on_end(&mut self, _ctx: &mut StrategyContext) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct MarketState {
    pub bars: BTreeMap<InstrumentId, Bar>,
    pub quotes: BTreeMap<InstrumentId, Quote>,
    pub marks: BTreeMap<InstrumentId, Price>,
}

impl MarketState {
    pub fn update(&mut self, event: &MarketEvent) {
        match event {
            MarketEvent::Bar(bar) => {
                self.marks.insert(bar.instrument_id, bar.close);
                self.bars.insert(bar.instrument_id, bar.clone());
            }
            MarketEvent::Quote(quote) => {
                self.marks.insert(
                    quote.instrument_id,
                    (quote.bid_price + quote.ask_price) / 2.0,
                );
                self.quotes.insert(quote.instrument_id, quote.clone());
            }
            MarketEvent::Trade(trade) => {
                self.marks.insert(trade.instrument_id, trade.price);
            }
            MarketEvent::Mark(mark) => {
                self.marks.insert(mark.instrument_id, mark.price);
            }
            _ => {}
        }
    }
}

pub struct StrategyContext<'a> {
    now: TimestampNs,
    market_state: &'a MarketState,
    audit: &'a mut AuditTrail,
    orders: Vec<OrderRequest>,
}

impl<'a> StrategyContext<'a> {
    pub fn new(now: TimestampNs, market_state: &'a MarketState, audit: &'a mut AuditTrail) -> Self {
        Self {
            now,
            market_state,
            audit,
            orders: Vec::new(),
        }
    }

    pub fn now(&self) -> TimestampNs {
        self.now
    }

    pub fn latest_bar(&mut self, instrument_id: InstrumentId, feature_name: &str) -> Option<Bar> {
        let bar = self.market_state.bars.get(&instrument_id)?.clone();
        let cutoff = bar.feature_cutoff_ts.unwrap_or(bar.ts_close);
        self.audit
            .record_feature_cutoff(feature_name, cutoff, self.now)
            .ok()?;
        Some(bar)
    }

    pub fn latest_quote(
        &mut self,
        instrument_id: InstrumentId,
        feature_name: &str,
    ) -> Option<Quote> {
        let quote = self.market_state.quotes.get(&instrument_id)?.clone();
        self.audit
            .record_feature_cutoff(feature_name, quote.ts, self.now)
            .ok()?;
        Some(quote)
    }

    pub fn mark(&self, instrument_id: InstrumentId) -> Option<Price> {
        self.market_state.marks.get(&instrument_id).copied()
    }

    pub fn record_feature_cutoff(
        &mut self,
        name: impl Into<String>,
        cutoff_ts: TimestampNs,
    ) -> Result<()> {
        self.audit
            .record_feature_cutoff(name, cutoff_ts, self.now)
            .map_err(anyhow::Error::new)
    }

    pub fn record_signal_before_entry(
        &mut self,
        name: impl Into<String>,
        signal_ts: TimestampNs,
        entry_ts: TimestampNs,
    ) -> Result<()> {
        self.audit
            .check_signal_before_entry(name, signal_ts, entry_ts)
            .map_err(anyhow::Error::new)
    }

    pub fn submit_order(&mut self, mut order: OrderRequest) {
        order.created_ts = self.now;
        self.orders.push(order);
    }

    pub fn drain_orders(&mut self) -> Vec<OrderRequest> {
        std::mem::take(&mut self.orders)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSection {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortfolioConfig {
    pub initial_capital: f64,
    pub base_currency: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub name: String,
    #[serde(default)]
    pub crate_path: Option<PathBuf>,
    #[serde(default)]
    pub params: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionConfig {
    #[serde(default = "default_fill_policy_name")]
    pub policy: String,
}

fn default_fill_policy_name() -> String {
    "conservative_causal".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ValidationConfig {
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputConfig {
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunManifest {
    pub run: RunSection,
    #[serde(default)]
    pub data: Vec<DataSourceConfig>,
    #[serde(default)]
    pub universe: Vec<InstrumentSpec>,
    pub portfolio: PortfolioConfig,
    pub strategy: StrategyConfig,
    #[serde(default = "default_execution_config")]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub validation: ValidationConfig,
    pub output: OutputConfig,
}

fn default_execution_config() -> ExecutionConfig {
    ExecutionConfig {
        policy: default_fill_policy_name(),
    }
}

impl RunManifest {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest = toml::from_str::<RunManifest>(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        Ok(manifest)
    }

    pub fn validation_mode(&self) -> ValidationMode {
        if self.validation.strict {
            ValidationMode::Strict
        } else {
            ValidationMode::Warn
        }
    }

    pub fn lock_hash(&self) -> Result<String> {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(b"kenoma-manifest-lock-v1\0");
        hasher.update(serde_json::to_vec(self)?);
        for source in &self.data {
            hasher.update(b"\0data-source\0");
            hasher.update(source.path.to_string_lossy().as_bytes());
            let bytes = fs::read(&source.path)
                .with_context(|| format!("hashing data source {}", source.path.display()))?;
            hasher.update(Sha256::digest(&bytes));
        }
        Ok(format!("{:x}", hasher.finalize()))
    }
}

pub struct BacktestEngine<S> {
    strategy: S,
    manifest: RunManifest,
    instruments: BTreeMap<InstrumentId, InstrumentSpec>,
    portfolio: Portfolio,
    fill_model: ConservativeCausalFillModel,
    audit: AuditTrail,
    market_state: MarketState,
    pending_orders: Vec<OrderRequest>,
    orders: Vec<OrderRequest>,
    fills: Vec<Fill>,
    equity_curve: Vec<EquityPoint>,
    last_borrow_ts: BTreeMap<InstrumentId, TimestampNs>,
    last_funding_ts: BTreeMap<InstrumentId, TimestampNs>,
    next_order_id: u64,
}

impl<S: Strategy> BacktestEngine<S> {
    pub fn new(strategy: S, manifest: RunManifest) -> Self {
        let instruments = manifest
            .universe
            .iter()
            .map(|spec| (spec.id, spec.clone()))
            .collect::<BTreeMap<_, _>>();
        let portfolio = Portfolio::new(
            manifest.portfolio.base_currency.clone(),
            manifest.portfolio.initial_capital,
        );
        let audit = AuditTrail::new(manifest.validation_mode());
        Self {
            strategy,
            manifest,
            instruments,
            portfolio,
            fill_model: ConservativeCausalFillModel::default(),
            audit,
            market_state: MarketState::default(),
            pending_orders: Vec::new(),
            orders: Vec::new(),
            fills: Vec::new(),
            equity_curve: Vec::new(),
            last_borrow_ts: BTreeMap::new(),
            last_funding_ts: BTreeMap::new(),
            next_order_id: 1,
        }
    }

    pub fn run_manifest(strategy: S, manifest: RunManifest) -> Result<(RunReport, AuditTrail)> {
        let events = load_sources(&manifest.data)?;
        let mut engine = Self::new(strategy, manifest);
        let report = engine.run(events)?;
        Ok((report, engine.audit))
    }

    pub fn run(&mut self, mut events: Vec<MarketEvent>) -> Result<RunReport> {
        events.sort_by_key(|event| (event.timestamp_ns(), event.priority()));
        for event in &events {
            self.audit_market_event_cutoff(event)?;
            self.market_state.update(event);
            self.apply_marks();
            self.accrue_financing(event.timestamp_ns())?;
            self.evaluate_pending_orders(event)?;
            self.dispatch_strategy_event(event)?;
            self.record_equity(event.timestamp_ns())?;
        }
        let end_ts = events.last().map(MarketEvent::timestamp_ns).unwrap_or(0);
        let state_snapshot = self.market_state.clone();
        let audit_events_before = self.audit.events.len();
        let new_orders = {
            let mut ctx = StrategyContext::new(end_ts, &state_snapshot, &mut self.audit);
            self.strategy.on_end(&mut ctx)?;
            ctx.drain_orders()
        };
        self.fail_on_new_strict_warnings(audit_events_before)?;
        self.accept_context_orders(new_orders);
        self.record_equity(end_ts)?;
        Ok(self.report())
    }

    pub fn write_artifacts(&self) -> Result<()> {
        write_run_artifacts(
            &self.manifest,
            &self.audit,
            &self.report(),
            &self.manifest.output.dir,
        )
    }

    pub fn audit(&self) -> &AuditTrail {
        &self.audit
    }

    fn dispatch_strategy_event(&mut self, event: &MarketEvent) -> Result<()> {
        let state_snapshot = self.market_state.clone();
        let audit_events_before = self.audit.events.len();
        let new_orders = {
            let mut ctx =
                StrategyContext::new(event.timestamp_ns(), &state_snapshot, &mut self.audit);
            match event {
                MarketEvent::Timer(timer) => self.strategy.on_timer(&mut ctx, &timer.name)?,
                _ => self.strategy.on_event(&mut ctx, event)?,
            }
            ctx.drain_orders()
        };
        self.fail_on_new_strict_warnings(audit_events_before)?;
        self.accept_context_orders(new_orders);
        Ok(())
    }

    fn accept_context_orders(&mut self, orders: Vec<OrderRequest>) {
        for mut order in orders {
            if order.id == 0 {
                order.id = self.next_order_id;
                self.next_order_id += 1;
            }
            self.orders.push(order.clone());
            self.pending_orders.push(order);
        }
    }

    fn evaluate_pending_orders(&mut self, event: &MarketEvent) -> Result<()> {
        let mut remaining = Vec::new();
        let mut fills = Vec::new();
        for order in self.pending_orders.drain(..) {
            let Some(spec) = self.instruments.get(&order.instrument_id) else {
                remaining.push(order);
                continue;
            };
            if let Some(fill) = self.fill_model.try_fill(&order, event, spec) {
                fills.push(fill);
            } else {
                remaining.push(order);
            }
        }
        self.pending_orders = remaining;
        let had_fills = !fills.is_empty();
        for fill in fills {
            self.apply_fill(fill)?;
        }
        if had_fills {
            self.apply_marks();
        }
        Ok(())
    }

    fn apply_fill(&mut self, fill: Fill) -> Result<()> {
        let spec = self
            .instruments
            .get(&fill.instrument_id)
            .with_context(|| format!("missing instrument {}", fill.instrument_id))?
            .clone();
        self.portfolio.apply_fill(&fill, &spec)?;
        let state_snapshot = self.market_state.clone();
        let audit_events_before = self.audit.events.len();
        let new_orders = {
            let mut ctx = StrategyContext::new(fill.ts, &state_snapshot, &mut self.audit);
            self.strategy.on_fill(&mut ctx, &fill)?;
            ctx.drain_orders()
        };
        self.fail_on_new_strict_warnings(audit_events_before)?;
        self.accept_context_orders(new_orders);
        self.fills.push(fill);
        Ok(())
    }

    fn audit_market_event_cutoff(&mut self, event: &MarketEvent) -> Result<()> {
        if let MarketEvent::Bar(bar) = event {
            let cutoff = bar.feature_cutoff_ts.unwrap_or(bar.ts_close);
            self.audit
                .record_feature_cutoff(
                    format!("bar_event:{}", bar.instrument_id),
                    cutoff,
                    event.timestamp_ns(),
                )
                .map_err(anyhow::Error::new)?;
        }
        Ok(())
    }

    fn fail_on_new_strict_warnings(&self, audit_events_before: usize) -> Result<()> {
        if self.audit.mode != ValidationMode::Strict {
            return Ok(());
        }
        if let Some(event) = self.audit.events[audit_events_before..]
            .iter()
            .find(|event| event.severity == AuditSeverity::Warning)
        {
            anyhow::bail!("{}", event.message);
        }
        Ok(())
    }

    fn apply_marks(&mut self) {
        for (&instrument_id, &price) in &self.market_state.marks {
            self.portfolio.mark(instrument_id, price);
        }
    }

    fn accrue_financing(&mut self, ts: TimestampNs) -> Result<()> {
        for (&instrument_id, spec) in &self.instruments {
            if let Some(funding) = &spec.funding {
                if funding.interval_ns > 0 {
                    let last_ts = self.last_funding_ts.entry(instrument_id).or_insert(ts);
                    let intervals = ts.saturating_sub(*last_ts) / funding.interval_ns;
                    if intervals > 0 {
                        self.portfolio
                            .accrue_funding(
                                instrument_id,
                                spec,
                                funding.rate_per_interval * intervals as f64,
                            )
                            .map_err(anyhow::Error::new)?;
                        *last_ts += intervals * funding.interval_ns;
                    }
                }
            }

            if let Some(borrow) = &spec.borrow {
                let last_ts = self.last_borrow_ts.entry(instrument_id).or_insert(ts);
                if ts <= *last_ts {
                    continue;
                }
                let days = (ts - *last_ts) as f64 / 86_400_000_000_000.0;
                if days > 0.0 {
                    self.portfolio
                        .accrue_borrow(instrument_id, spec, borrow.annualized_rate, days)
                        .map_err(anyhow::Error::new)?;
                }
                *last_ts = ts;
            }
        }
        Ok(())
    }

    fn record_equity(&mut self, ts: TimestampNs) -> Result<()> {
        let equity = self.portfolio.equity(&self.instruments)?;
        if self
            .equity_curve
            .last()
            .is_none_or(|point| point.ts != ts || (point.equity - equity).abs() > 1e-9)
        {
            self.equity_curve.push(EquityPoint { ts, equity });
        }
        Ok(())
    }

    fn report(&self) -> RunReport {
        let stats_curve = self
            .equity_curve
            .iter()
            .map(|point| StatsEquityPoint {
                ts: point.ts,
                equity: point.equity,
            })
            .collect::<Vec<_>>();
        let trade_pnls = realized_trade_pnls(&self.fills, &self.instruments);
        let metrics = compute_metrics(&stats_curve, &trade_pnls);
        RunReport {
            run_id: self.manifest.run.id.clone(),
            metrics: RunMetrics {
                start_equity: metrics.start_equity,
                end_equity: metrics.end_equity,
                total_return: metrics.total_return,
                max_drawdown: metrics.max_drawdown,
                sharpe: metrics.sharpe,
                profit_factor: metrics.profit_factor,
                total_fees: self.portfolio.total_fees,
                trade_count: metrics.trade_count,
            },
            orders: self.orders.clone(),
            fills: self.fills.clone(),
            positions: self.portfolio.positions_vec(),
            equity_curve: self.equity_curve.clone(),
        }
    }
}

fn realized_trade_pnls(
    fills: &[Fill],
    instruments: &BTreeMap<InstrumentId, InstrumentSpec>,
) -> Vec<TradePnl> {
    let mut positions = BTreeMap::<InstrumentId, (f64, f64)>::new();
    let mut trade_pnls = Vec::new();

    for fill in fills {
        let multiplier = instruments
            .get(&fill.instrument_id)
            .map(|spec| spec.multiplier)
            .unwrap_or(1.0);
        let signed_qty = fill.qty * fill.side.sign();
        let (qty, avg_price) = positions.entry(fill.instrument_id).or_insert((0.0, 0.0));
        let old_qty = *qty;
        let mut realized = 0.0;

        if old_qty == 0.0 || old_qty.signum() == signed_qty.signum() {
            let new_qty = old_qty + signed_qty;
            *avg_price = if new_qty == 0.0 {
                0.0
            } else {
                (old_qty.abs() * *avg_price + signed_qty.abs() * fill.price) / new_qty.abs()
            };
            *qty = new_qty;
        } else {
            let close_qty = old_qty.abs().min(signed_qty.abs());
            realized = close_qty * (fill.price - *avg_price) * old_qty.signum() * multiplier;
            let new_qty = old_qty + signed_qty;
            *qty = new_qty;
            *avg_price = if new_qty == 0.0 {
                0.0
            } else if old_qty.signum() != new_qty.signum() {
                fill.price
            } else {
                *avg_price
            };
        }

        let pnl = realized - fill.fee;
        if pnl != 0.0 {
            trade_pnls.push(TradePnl { ts: fill.ts, pnl });
        }
    }

    trade_pnls
}

pub fn write_run_artifacts(
    manifest: &RunManifest,
    audit: &AuditTrail,
    report: &RunReport,
    run_dir: impl AsRef<Path>,
) -> Result<()> {
    let run_dir = run_dir.as_ref();
    fs::create_dir_all(run_dir)?;
    let lock = serde_json::json!({
        "run_id": manifest.run.id,
        "manifest_hash": manifest.lock_hash()?,
        "manifest": manifest,
    });
    write_json(run_dir.join("manifest.lock.json"), &lock)?;
    write_json(run_dir.join("audit.json"), audit)?;
    write_json(run_dir.join("metrics.json"), &report.metrics)?;
    write_json(run_dir.join("orders.json"), &report.orders)?;
    write_json(run_dir.join("fills.json"), &report.fills)?;
    write_json(run_dir.join("positions.json"), &report.positions)?;
    write_json(run_dir.join("equity_curve.json"), &report.equity_curve)?;
    write_orders_parquet(run_dir.join("orders.parquet"), &report.orders)?;
    write_fills_parquet(run_dir.join("fills.parquet"), &report.fills)?;
    write_positions_parquet(run_dir.join("positions.parquet"), &report.positions)?;
    write_equity_curve_parquet(run_dir.join("equity_curve.parquet"), &report.equity_curve)?;
    Ok(())
}

fn write_json(path: PathBuf, value: &impl Serialize) -> Result<()> {
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(file), value)?;
    Ok(())
}

#[derive(Debug, Default)]
pub struct NoopStrategy;

impl Strategy for NoopStrategy {}

#[derive(Debug, Clone)]
pub struct BuyFirstBarStrategy {
    pub instrument_id: InstrumentId,
    pub qty: f64,
    entered: bool,
}

impl BuyFirstBarStrategy {
    pub fn new(instrument_id: InstrumentId, qty: f64) -> Self {
        Self {
            instrument_id,
            qty,
            entered: false,
        }
    }
}

impl Strategy for BuyFirstBarStrategy {
    fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
        if self.entered {
            return Ok(());
        }
        if matches!(event, MarketEvent::Bar(bar) if bar.instrument_id == self.instrument_id)
            || matches!(event, MarketEvent::Quote(quote) if quote.instrument_id == self.instrument_id)
        {
            ctx.submit_order(OrderRequest::market(
                self.instrument_id,
                kenoma_types::OrderSide::Buy,
                self.qty,
            ));
            self.entered = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kenoma_types::{AssetClass, BorrowSpec, FeeSpec, OrderSide, TimerEvent};

    fn spec() -> InstrumentSpec {
        InstrumentSpec {
            id: 1,
            symbol: "ABC".to_string(),
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
            metadata: Default::default(),
        }
    }

    fn manifest() -> RunManifest {
        RunManifest {
            run: RunSection {
                id: "test".to_string(),
            },
            data: Vec::new(),
            universe: vec![spec()],
            portfolio: PortfolioConfig {
                initial_capital: 1_000.0,
                base_currency: "USD".to_string(),
            },
            strategy: StrategyConfig {
                name: "test".to_string(),
                crate_path: None,
                params: BTreeMap::new(),
            },
            execution: ExecutionConfig {
                policy: "conservative_causal".to_string(),
            },
            validation: ValidationConfig { strict: false },
            output: OutputConfig {
                dir: PathBuf::from("target/test-run"),
            },
        }
    }

    #[derive(Default)]
    struct SubmitOnFirstBar {
        done: bool,
    }

    impl Strategy for SubmitOnFirstBar {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if !self.done && matches!(event, MarketEvent::Bar(_)) {
                ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
                self.done = true;
            }
            Ok(())
        }
    }

    #[test]
    fn engine_enforces_next_bar_market_fill() {
        let events = vec![
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(10),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 10,
                ts_close: 20,
                open: 101.0,
                high: 101.0,
                low: 101.0,
                close: 101.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(20),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 20,
                ts_close: 30,
                open: 102.0,
                high: 102.0,
                low: 102.0,
                close: 102.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(30),
            }),
        ];
        let mut engine = BacktestEngine::new(SubmitOnFirstBar::default(), manifest());
        let report = engine.run(events).unwrap();
        assert_eq!(report.orders.len(), 1);
        assert_eq!(report.fills.len(), 1);
        assert_eq!(report.fills[0].ts, 30);
    }

    #[test]
    fn fill_does_not_overwrite_current_market_mark() {
        let events = vec![
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(10),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 10,
                ts_close: 20,
                open: 101.0,
                high: 102.0,
                low: 101.0,
                close: 102.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(20),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 20,
                ts_close: 30,
                open: 102.0,
                high: 103.0,
                low: 102.0,
                close: 103.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(30),
            }),
        ];
        let mut engine = BacktestEngine::new(SubmitOnFirstBar::default(), manifest());
        let report = engine.run(events).unwrap();
        let equity_at_bar_close = report.equity_curve.last().unwrap().equity;
        assert!((equity_at_bar_close - 1000.99).abs() < 1e-9);
    }

    struct ReadBarOnTimer;

    impl Strategy for ReadBarOnTimer {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            let _ = ctx.latest_bar(1, "timer_bar");
            Ok(())
        }
    }

    #[test]
    fn strict_accessor_warning_is_not_swallowed() {
        let mut strict_manifest = manifest();
        strict_manifest.validation.strict = true;
        let mut engine = BacktestEngine::new(ReadBarOnTimer, strict_manifest);
        engine.market_state.bars.insert(
            1,
            Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 101.0,
                high: 101.0,
                low: 101.0,
                close: 101.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(20),
            },
        );
        let events = vec![MarketEvent::Timer(TimerEvent {
            ts: 15,
            name: "check".to_string(),
        })];
        let err = engine.run(events).unwrap_err();
        assert!(err.to_string().contains("timer_bar"));
    }

    #[test]
    fn strict_future_bar_event_cutoff_fails_before_dispatch() {
        let events = vec![MarketEvent::Bar(Bar {
            instrument_id: 1,
            ts_open: 0,
            ts_close: 10,
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 1.0,
            vwap: None,
            feature_cutoff_ts: Some(20),
        })];
        let mut strict_manifest = manifest();
        strict_manifest.validation.strict = true;
        let mut engine = BacktestEngine::new(NoopStrategy, strict_manifest);
        let err = engine.run(events).unwrap_err();
        assert!(err.to_string().contains("bar_event:1"));
    }

    #[test]
    fn realized_trade_pnls_include_price_pnl() {
        let mut instruments = BTreeMap::new();
        instruments.insert(1, spec());
        let fills = vec![
            Fill {
                order_id: 1,
                instrument_id: 1,
                ts: 1,
                side: OrderSide::Buy,
                price: 100.0,
                qty: 1.0,
                fee: 0.0,
                liquidity: None,
            },
            Fill {
                order_id: 2,
                instrument_id: 1,
                ts: 2,
                side: OrderSide::Sell,
                price: 110.0,
                qty: 1.0,
                fee: 0.0,
                liquidity: None,
            },
        ];
        let pnls = realized_trade_pnls(&fills, &instruments);
        assert_eq!(pnls, vec![TradePnl { ts: 2, pnl: 10.0 }]);
    }

    #[test]
    fn context_future_feature_warning_is_written() {
        let mut audit = AuditTrail::new(ValidationMode::Warn);
        let state = MarketState::default();
        let mut ctx = StrategyContext::new(10, &state, &mut audit);
        ctx.record_feature_cutoff("bad", 11).unwrap();
        assert_eq!(ctx.audit.warnings().count(), 1);
    }

    #[test]
    fn artifacts_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = manifest();
        manifest.output.dir = dir.path().to_path_buf();
        let engine = BacktestEngine::new(NoopStrategy, manifest);
        engine.write_artifacts().unwrap();
        assert!(dir.path().join("manifest.lock.json").is_file());
        assert!(dir.path().join("audit.json").is_file());
    }

    #[test]
    fn manifest_lock_hash_changes_when_data_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("bars.csv");
        std::fs::write(
            &data_path,
            "instrument_id,ts_open,ts_close,open,high,low,close,volume\n1,0,1,1,1,1,1,1\n",
        )
        .unwrap();
        let mut manifest = manifest();
        manifest.data = vec![DataSourceConfig {
            kind: kenoma_data::DataSourceKind::BarCsv,
            path: data_path.clone(),
            instrument_id: None,
            date: None,
        }];
        let first_hash = manifest.lock_hash().unwrap();
        std::fs::write(
            &data_path,
            "instrument_id,ts_open,ts_close,open,high,low,close,volume\n1,0,1,2,2,2,2,1\n",
        )
        .unwrap();
        let second_hash = manifest.lock_hash().unwrap();
        assert_ne!(first_hash, second_hash);
    }

    #[derive(Default)]
    struct ShortFirstBar {
        done: bool,
    }

    impl Strategy for ShortFirstBar {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if !self.done && matches!(event, MarketEvent::Bar(_)) {
                ctx.submit_order(OrderRequest::market(1, OrderSide::Sell, 10.0));
                self.done = true;
            }
            Ok(())
        }
    }

    #[test]
    fn engine_accrues_short_borrow_between_events() {
        let mut borrow_spec = spec();
        borrow_spec.tick_size = 0.0;
        borrow_spec.borrow = Some(BorrowSpec {
            annualized_rate: 0.365,
        });
        let mut manifest = manifest();
        manifest.universe = vec![borrow_spec];
        let one_day = 86_400_000_000_000;
        let events = vec![
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(10),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 20,
                ts_close: 30,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(30),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: one_day + 20,
                ts_close: one_day + 30,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 1.0,
                vwap: None,
                feature_cutoff_ts: Some(one_day + 30),
            }),
        ];
        let mut engine = BacktestEngine::new(ShortFirstBar::default(), manifest);
        let report = engine.run(events).unwrap();
        assert_eq!(report.fills.len(), 1);
        assert!((report.metrics.end_equity - 999.0).abs() < 1e-9);
    }
}
