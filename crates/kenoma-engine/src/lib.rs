//! Timestamp-ordered backtest engine, strategy API, manifests, and artifacts.

use anyhow::{Context as AnyhowContext, Result};
use kenoma_audit::{AuditSeverity, AuditTrail, ValidationMode};
use kenoma_book::BookBuilder;
use kenoma_data::{load_sources, DataSourceConfig};
use kenoma_data::{
    write_equity_curve_parquet, write_fills_parquet, write_orders_parquet, write_positions_parquet,
};
use kenoma_execution::{
    BarFillMode, ConservativeCausalFillModel, ExecutionCosts, MboLimitFillTracker,
};
use kenoma_portfolio::Portfolio;
use kenoma_stats::{
    compute_metrics_with, EquityPoint as StatsEquityPoint, MetricsConfig, TradePnl,
};
use kenoma_types::{
    Bar, EquityPoint, Fill, InstrumentId, InstrumentSpec, MarketEvent, OrderRequest, Price, Quote,
    RunMetrics, RunReport, SessionPhase, TimestampNs,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

mod resolvers;
pub use resolvers::{AlwaysRth, RolloverResolver, SessionResolver, StaticContract};

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

    /// Fired by the engine when the resolved session phase changes for an
    /// instrument, AFTER `on_event`/`on_timer` for the current event has
    /// returned and any orders it submitted have been drained.
    ///
    /// Only fired when `ExecutionConfig.enable_hg_hooks == true`. Default
    /// impl is a no-op so existing strategies need not override.
    fn on_session_boundary(
        &mut self,
        _ctx: &mut StrategyContext,
        _instrument_id: InstrumentId,
        _new_phase: SessionPhase,
    ) -> Result<()> {
        Ok(())
    }

    /// Fired by the engine when the resolved active contract changes for an
    /// instrument family, AFTER `on_event`/`on_timer` for the current event
    /// has returned and any orders it submitted have been drained.
    ///
    /// The engine queues a force-flat market order with
    /// `reason="ROLLOVER_BOUNDARY"` for any open position on `old_contract`
    /// AFTER this method returns and its orders are drained -- the strategy's
    /// own flattening (if any) goes first, the engine's safety net second.
    ///
    /// Only fired when `ExecutionConfig.enable_hg_hooks == true`. Default
    /// impl is a no-op.
    fn on_rollover_boundary(
        &mut self,
        _ctx: &mut StrategyContext,
        _instrument_family: &str,
        _old_contract: &str,
        _new_contract: &str,
    ) -> Result<()> {
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
        let cutoff = effective_cutoff_ts(&bar);
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
    #[serde(default)]
    pub bar_fill_mode: BarFillMode,
    #[serde(default = "default_fixed_spread_ticks")]
    pub fixed_spread_ticks: f64,
    #[serde(default)]
    pub commission_per_side: f64,
    #[serde(default)]
    pub slippage_ticks: f64,
    /// Opt-in switch for the hunger-games harness hooks.
    ///
    /// When `false` (the default), `on_session_boundary` and
    /// `on_rollover_boundary` are NEVER fired and the engine NEVER consults
    /// the session or rollover resolvers, even if they have been set via
    /// `BacktestEngine::with_session_resolver` /
    /// `BacktestEngine::with_rollover_resolver`. This guarantees that
    /// existing consumers (es-sr-canvas, kenoma-fx, anything depending on
    /// kenoma-backtester at v0.1.x) see byte-identical behaviour to baseline.
    ///
    /// When `true`, the engine consults the resolvers per event and fires
    /// the boundary hooks on phase / active-contract changes.
    #[serde(default)]
    pub enable_hg_hooks: bool,
}

fn default_fill_policy_name() -> String {
    "conservative_causal".to_string()
}

fn default_fixed_spread_ticks() -> f64 {
    1.0
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            policy: default_fill_policy_name(),
            bar_fill_mode: BarFillMode::default(),
            fixed_spread_ticks: default_fixed_spread_ticks(),
            commission_per_side: 0.0,
            slippage_ticks: 0.0,
            enable_hg_hooks: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ValidationConfig {
    #[serde(default)]
    pub strict: bool,
}

/// Run-level metrics configuration.
///
/// `annualization_factor` is the number of equity-curve return periods per
/// year. When set, the engine reports an `annualized_sharpe` alongside the
/// per-period Sharpe. Leave it `None` (the default) for runs whose return
/// frequency is unspecified or irregular — annualizing per-event returns by
/// `sqrt(252)` is meaningless except for daily bars.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricsManifestConfig {
    #[serde(default)]
    pub annualization_factor: Option<f64>,
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
    #[serde(default)]
    pub metrics: MetricsManifestConfig,
    pub output: OutputConfig,
}

fn default_execution_config() -> ExecutionConfig {
    ExecutionConfig::default()
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
    mbo_books: BTreeMap<InstrumentId, BookBuilder>,
    pending_orders: Vec<OrderRequest>,
    mbo_limit_trackers: Vec<MboLimitFillTracker>,
    orders: Vec<OrderRequest>,
    fills: Vec<Fill>,
    equity_curve: Vec<EquityPoint>,
    last_borrow_ts: BTreeMap<InstrumentId, TimestampNs>,
    last_funding_ts: BTreeMap<InstrumentId, TimestampNs>,
    next_order_id: u64,
    session_resolver: Box<dyn SessionResolver>,
    rollover_resolver: Box<dyn RolloverResolver>,
    // Read by dispatch_session_boundary / dispatch_rollover_boundary (Task 8–9).
    #[allow(dead_code)]
    last_session_phase: BTreeMap<InstrumentId, SessionPhase>,
    #[allow(dead_code)]
    last_active_contract: BTreeMap<String, String>,
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
        let costs = ExecutionCosts {
            commission_per_side: manifest.execution.commission_per_side,
            spread_model: kenoma_execution::SpreadModel::Fixed,
            fixed_spread_ticks: manifest.execution.fixed_spread_ticks,
            slippage_ticks: manifest.execution.slippage_ticks,
            bar_fill_mode: manifest.execution.bar_fill_mode,
        };
        let fill_model = ConservativeCausalFillModel {
            policy: kenoma_execution::FillPolicy::ConservativeCausal,
            costs,
        };
        Self {
            strategy,
            manifest,
            instruments,
            portfolio,
            fill_model,
            audit,
            market_state: MarketState::default(),
            mbo_books: BTreeMap::new(),
            pending_orders: Vec::new(),
            mbo_limit_trackers: Vec::new(),
            orders: Vec::new(),
            fills: Vec::new(),
            equity_curve: Vec::new(),
            last_borrow_ts: BTreeMap::new(),
            last_funding_ts: BTreeMap::new(),
            next_order_id: 1,
            session_resolver: Box::new(AlwaysRth),
            rollover_resolver: Box::new(StaticContract::new("__static__")),
            last_session_phase: BTreeMap::new(),
            last_active_contract: BTreeMap::new(),
        }
    }

    pub fn with_session_resolver(mut self, resolver: Box<dyn SessionResolver>) -> Self {
        self.session_resolver = resolver;
        self
    }

    pub fn with_rollover_resolver(mut self, resolver: Box<dyn RolloverResolver>) -> Self {
        self.rollover_resolver = resolver;
        self
    }

    pub fn run_manifest(strategy: S, manifest: RunManifest) -> Result<(RunReport, AuditTrail)> {
        let events = load_sources(&manifest.data)?;
        let mut engine = Self::new(strategy, manifest);
        let report = engine.run(events)?;
        Ok((report, engine.audit))
    }

    pub fn run(&mut self, mut events: Vec<MarketEvent>) -> Result<RunReport> {
        self.validate_execution_policy()?;
        events.sort_by_key(|event| (event.timestamp_ns(), event.priority()));
        for event in &events {
            self.audit_market_event_cutoff(event)?;
            self.market_state.update(event);
            self.apply_marks();
            self.accrue_financing(event.timestamp_ns())?;
            self.evaluate_pending_orders(event)?;
            self.update_mbo_book(event);
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
            } else {
                self.next_order_id = self.next_order_id.max(order.id.saturating_add(1));
            }
            self.orders.push(order.clone());
            if let Some(mut tracker) = MboLimitFillTracker::new(order.clone()) {
                if let Some(book) = self.mbo_books.get(&order.instrument_id) {
                    tracker.snapshot_queue_ahead(book);
                }
                self.mbo_limit_trackers.push(tracker);
            }
            self.pending_orders.push(order);
        }
    }

    fn evaluate_pending_orders(&mut self, event: &MarketEvent) -> Result<()> {
        let mut fills = Vec::new();
        let mut filled_order_ids = BTreeSet::new();

        if let MarketEvent::Mbo(mbo) = event {
            if let Some(book) = self.mbo_books.get(&mbo.instrument_id) {
                let mut remaining_trackers = Vec::new();
                for mut tracker in self.mbo_limit_trackers.drain(..) {
                    if tracker.order.instrument_id != mbo.instrument_id {
                        remaining_trackers.push(tracker);
                        continue;
                    }
                    if tracker.observe(book, mbo) {
                        let Some(spec) = self.instruments.get(&tracker.order.instrument_id) else {
                            remaining_trackers.push(tracker);
                            continue;
                        };
                        if let Some(fill) = tracker.to_fill(spec, &self.fill_model.costs) {
                            filled_order_ids.insert(fill.order_id);
                            fills.push(fill);
                        }
                    } else {
                        remaining_trackers.push(tracker);
                    }
                }
                self.mbo_limit_trackers = remaining_trackers;
            }
        } else {
            let mut remaining = Vec::new();
            let active_mbo_tracked_order_ids = self.active_mbo_tracked_order_ids();
            for order in self.pending_orders.drain(..) {
                if active_mbo_tracked_order_ids.contains(&order.id) {
                    remaining.push(order);
                    continue;
                }
                let Some(spec) = self.instruments.get(&order.instrument_id) else {
                    remaining.push(order);
                    continue;
                };
                if let Some(fill) = self.fill_model.try_fill(&order, event, spec) {
                    filled_order_ids.insert(fill.order_id);
                    fills.push(fill);
                } else {
                    remaining.push(order);
                }
            }
            self.pending_orders = remaining;
        }
        if !filled_order_ids.is_empty() {
            self.pending_orders
                .retain(|order| !filled_order_ids.contains(&order.id));
            self.mbo_limit_trackers
                .retain(|tracker| !filled_order_ids.contains(&tracker.order.id));
        }
        let had_fills = !fills.is_empty();
        for fill in fills {
            self.apply_fill(fill)?;
        }
        if had_fills {
            self.apply_marks();
        }
        Ok(())
    }

    fn active_mbo_tracked_order_ids(&self) -> BTreeSet<u64> {
        self.mbo_limit_trackers
            .iter()
            .filter(|tracker| {
                tracker.queue_ahead.is_some()
                    || self.mbo_books.contains_key(&tracker.order.instrument_id)
            })
            .map(|tracker| tracker.order.id)
            .collect()
    }

    fn update_mbo_book(&mut self, event: &MarketEvent) {
        if let MarketEvent::Mbo(mbo) = event {
            self.mbo_books
                .entry(mbo.instrument_id)
                .or_insert_with(|| BookBuilder::new(mbo.instrument_id))
                .process_mbo(mbo);
        }
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
            // Strict mode forbids implicit-default cutoffs: if the data
            // pipeline doesn't say when this bar's features became causal, the
            // audit can't catch a leak. The Warn-mode default of `ts_close` is
            // optimistic — silent acceptance lets fictional alpha through.
            if bar.feature_cutoff_ts.is_none() && self.audit.mode == ValidationMode::Strict {
                self.audit
                    .warn(
                        Some(event.timestamp_ns()),
                        kenoma_audit::AuditCode::FutureDataAccess,
                        format!(
                            "bar for instrument {} has no feature_cutoff_ts; \
                             strict mode requires explicit cutoff",
                            bar.instrument_id
                        ),
                    )
                    .map_err(anyhow::Error::new)?;
            }
            let cutoff = effective_cutoff_ts(bar);
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

    fn validate_execution_policy(&self) -> Result<()> {
        if self.manifest.execution.policy != "conservative_causal" {
            anyhow::bail!(
                "unsupported execution policy '{}'; supported policy is conservative_causal",
                self.manifest.execution.policy
            );
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
        let metrics = compute_metrics_with(
            &stats_curve,
            &trade_pnls,
            &MetricsConfig {
                annualization_factor: self.manifest.metrics.annualization_factor,
            },
        );
        RunReport {
            run_id: self.manifest.run.id.clone(),
            metrics: RunMetrics {
                start_equity: metrics.start_equity,
                end_equity: metrics.end_equity,
                total_return: metrics.total_return,
                max_drawdown: metrics.max_drawdown,
                sharpe: metrics.sharpe,
                sharpe_annualization: metrics.sharpe_annualization,
                annualized_sharpe: metrics.annualized_sharpe,
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

/// Resolves the causal cutoff timestamp for a bar.
///
/// When a bar declares an explicit `feature_cutoff_ts`, that's the contract.
/// Otherwise we fall back to `ts_close`, which is the most permissive
/// no-leak interpretation (i.e. the bar publishes nothing post-close). In
/// strict validation mode the engine warns separately when this fallback is
/// used so the silent default cannot mask a missing-cutoff data bug.
fn effective_cutoff_ts(bar: &Bar) -> TimestampNs {
    bar.feature_cutoff_ts.unwrap_or(bar.ts_close)
}

fn realized_trade_pnls(
    fills: &[Fill],
    instruments: &BTreeMap<InstrumentId, InstrumentSpec>,
) -> Vec<TradePnl> {
    #[derive(Debug, Clone, Copy, Default)]
    struct PnlPosition {
        qty: f64,
        avg_price: f64,
        open_fees: f64,
    }

    let mut positions = BTreeMap::<InstrumentId, PnlPosition>::new();
    let mut trade_pnls = Vec::new();

    for fill in fills {
        let multiplier = instruments
            .get(&fill.instrument_id)
            .map(|spec| spec.multiplier)
            .unwrap_or(1.0);
        let signed_qty = fill.qty * fill.side.sign();
        let position = positions.entry(fill.instrument_id).or_default();
        let old_qty = position.qty;

        if old_qty == 0.0 || old_qty.signum() == signed_qty.signum() {
            let new_qty = old_qty + signed_qty;
            position.avg_price = if new_qty == 0.0 {
                0.0
            } else {
                (old_qty.abs() * position.avg_price + signed_qty.abs() * fill.price) / new_qty.abs()
            };
            position.qty = new_qty;
            position.open_fees += fill.fee;
        } else {
            let close_qty = old_qty.abs().min(signed_qty.abs());
            let realized_price_pnl =
                close_qty * (fill.price - position.avg_price) * old_qty.signum() * multiplier;
            let entry_fee_alloc = if old_qty.abs() > 0.0 {
                position.open_fees * close_qty / old_qty.abs()
            } else {
                0.0
            };
            let close_fee_alloc = if fill.qty.abs() > 0.0 {
                fill.fee * close_qty / fill.qty.abs()
            } else {
                0.0
            };
            let pnl = realized_price_pnl - entry_fee_alloc - close_fee_alloc;
            if close_qty > 0.0 {
                trade_pnls.push(TradePnl { ts: fill.ts, pnl });
            }

            let new_qty = old_qty + signed_qty;
            position.qty = new_qty;
            position.open_fees -= entry_fee_alloc;
            position.avg_price = if new_qty == 0.0 {
                position.open_fees = 0.0;
                0.0
            } else if old_qty.signum() != new_qty.signum() {
                let opening_qty = signed_qty.abs() - close_qty;
                let opening_fee = fill.fee - close_fee_alloc;
                position.open_fees = opening_fee;
                debug_assert!(opening_qty > 0.0);
                fill.price
            } else {
                position.avg_price
            };
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
    use kenoma_types::{
        AssetClass, BorrowSpec, FeeSpec, MboEvent, OrderSide, Side, TimerEvent, Trade,
    };

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
            execution: ExecutionConfig::default(),
            validation: ValidationConfig { strict: false },
            metrics: MetricsManifestConfig::default(),
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

    #[derive(Default)]
    struct SubmitMboLimitOnce {
        done: bool,
    }

    impl Strategy for SubmitMboLimitOnce {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if !self.done && matches!(event, MarketEvent::Mbo(_)) {
                ctx.submit_order(OrderRequest::limit(1, OrderSide::Buy, 1.0, 100.0));
                self.done = true;
            }
            Ok(())
        }
    }

    #[test]
    fn engine_uses_mbo_queue_ahead_for_limit_fills() {
        let events = vec![
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                1,
                10,
                1,
                'A',
                'B',
                100_000_000_000,
                5,
                kenoma_book::F_LAST,
            )),
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                2,
                11,
                1,
                'F',
                'B',
                100_000_000_000,
                5,
                kenoma_book::F_LAST,
            )),
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                3,
                12,
                1,
                'F',
                'B',
                100_000_000_000,
                1,
                kenoma_book::F_LAST,
            )),
        ];
        let mut engine = BacktestEngine::new(SubmitMboLimitOnce::default(), manifest());
        let report = engine.run(events).unwrap();
        assert_eq!(report.orders.len(), 1);
        assert_eq!(report.fills.len(), 1);
        assert_eq!(report.fills[0].ts, 3);
        assert_eq!(report.fills[0].price, 100.0);
        assert_eq!(report.fills[0].liquidity.as_deref(), Some("maker"));
    }

    #[derive(Default)]
    struct SubmitTimerMboLimit {
        done: bool,
    }

    impl Strategy for SubmitTimerMboLimit {
        fn on_timer(&mut self, ctx: &mut StrategyContext, _name: &str) -> Result<()> {
            if !self.done {
                ctx.submit_order(OrderRequest::limit(1, OrderSide::Buy, 1.0, 100.0));
                self.done = true;
            }
            Ok(())
        }
    }

    #[test]
    fn mbo_limit_submitted_before_book_still_tracks_queue() {
        let events = vec![
            MarketEvent::Timer(TimerEvent {
                ts: 0,
                name: "submit".to_string(),
            }),
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                1,
                10,
                1,
                'A',
                'B',
                100_000_000_000,
                5,
                kenoma_book::F_LAST,
            )),
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                2,
                11,
                1,
                'F',
                'B',
                100_000_000_000,
                5,
                kenoma_book::F_LAST,
            )),
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                3,
                12,
                1,
                'F',
                'B',
                100_000_000_000,
                1,
                kenoma_book::F_LAST,
            )),
        ];
        let mut engine = BacktestEngine::new(SubmitTimerMboLimit::default(), manifest());
        let report = engine.run(events).unwrap();
        assert_eq!(report.orders.len(), 1);
        assert_eq!(report.fills.len(), 1);
        assert_eq!(report.fills[0].ts, 3);
    }

    #[test]
    fn mbo_tracked_limit_is_not_filled_by_trade_print_before_queue_depletes() {
        let events = vec![
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                1,
                10,
                1,
                'A',
                'B',
                100_000_000_000,
                10,
                kenoma_book::F_LAST,
            )),
            MarketEvent::Trade(Trade {
                instrument_id: 1,
                ts: 2,
                price: 100.0,
                size: 1.0,
                aggressor_side: Side::Ask,
            }),
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                3,
                11,
                1,
                'F',
                'B',
                100_000_000_000,
                10,
                kenoma_book::F_LAST,
            )),
            MarketEvent::Mbo(MboEvent::from_dbn_parts(
                4,
                12,
                1,
                'F',
                'B',
                100_000_000_000,
                1,
                kenoma_book::F_LAST,
            )),
        ];
        let mut engine = BacktestEngine::new(SubmitMboLimitOnce::default(), manifest());
        let report = engine.run(events).unwrap();
        assert_eq!(report.fills.len(), 1);
        assert_eq!(report.fills[0].ts, 4);
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
        // Use Idealized fills so that the third bar's market fill at `open`
        // is distinct from the bar close mark of 103 — that gap is what
        // exercises the mark-preservation property under test.
        let mut man = manifest();
        man.execution.bar_fill_mode = BarFillMode::Idealized;
        let mut engine = BacktestEngine::new(SubmitOnFirstBar::default(), man);
        let report = engine.run(events).unwrap();
        let equity_at_bar_close = report.equity_curve.last().unwrap().equity;
        assert!((equity_at_bar_close - 1000.99).abs() < 1e-9);
    }

    #[derive(Default)]
    struct ManualThenAutoOrderId {
        seen_quotes: usize,
    }

    impl Strategy for ManualThenAutoOrderId {
        fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
            if !matches!(event, MarketEvent::Quote(_)) {
                return Ok(());
            }
            self.seen_quotes += 1;
            if self.seen_quotes == 1 {
                let mut order = OrderRequest::market(1, OrderSide::Buy, 1.0);
                order.id = 1;
                ctx.submit_order(order);
            } else if self.seen_quotes == 2 {
                ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
            }
            Ok(())
        }
    }

    #[test]
    fn manual_order_id_advances_generated_id_counter() {
        let events = vec![
            MarketEvent::Quote(Quote {
                instrument_id: 1,
                ts: 1,
                bid_price: 99.0,
                bid_size: 10.0,
                ask_price: 100.0,
                ask_size: 10.0,
            }),
            MarketEvent::Quote(Quote {
                instrument_id: 1,
                ts: 2,
                bid_price: 100.0,
                bid_size: 10.0,
                ask_price: 101.0,
                ask_size: 10.0,
            }),
            MarketEvent::Quote(Quote {
                instrument_id: 1,
                ts: 3,
                bid_price: 101.0,
                bid_size: 10.0,
                ask_price: 102.0,
                ask_size: 10.0,
            }),
        ];
        let mut engine = BacktestEngine::new(ManualThenAutoOrderId::default(), manifest());
        let report = engine.run(events).unwrap();
        let order_ids = report
            .orders
            .iter()
            .map(|order| order.id)
            .collect::<Vec<_>>();
        assert_eq!(order_ids, vec![1, 2]);
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
    fn strict_mode_rejects_bar_with_missing_feature_cutoff_ts() {
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
            // None — strict mode must reject this.
            feature_cutoff_ts: None,
        })];
        let mut strict_manifest = manifest();
        strict_manifest.validation.strict = true;
        let mut engine = BacktestEngine::new(NoopStrategy, strict_manifest);
        let err = engine.run(events).unwrap_err();
        assert!(
            err.to_string().contains("no feature_cutoff_ts"),
            "expected missing-cutoff diagnostic, got: {err}"
        );
    }

    #[test]
    fn warn_mode_accepts_bar_with_missing_feature_cutoff_ts() {
        // Permissive default for backwards-compat: Warn mode silently falls
        // back to ts_close. Strict mode is the gate.
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
            feature_cutoff_ts: None,
        })];
        let mut engine = BacktestEngine::new(NoopStrategy, manifest());
        engine.run(events).unwrap();
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
    fn realized_trade_pnls_attach_entry_and_exit_fees_to_closed_trade() {
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
                fee: 1.0,
                liquidity: None,
            },
            Fill {
                order_id: 2,
                instrument_id: 1,
                ts: 2,
                side: OrderSide::Sell,
                price: 110.0,
                qty: 1.0,
                fee: 1.0,
                liquidity: None,
            },
        ];
        let pnls = realized_trade_pnls(&fills, &instruments);
        assert_eq!(pnls, vec![TradePnl { ts: 2, pnl: 8.0 }]);
    }

    #[test]
    fn engine_realized_trade_pnls_agree_with_portfolio_minus_fees_on_round_trip() {
        // Anti-divergence guard: the engine has TWO realized-PnL paths that
        // must stay consistent or downstream metrics misreport. After a full
        // round-trip back to flat:
        //   sum(realized_trade_pnls)  ==  position.realized_pnl - position.fees
        // Both should also equal end_equity - start_equity for a no-funding,
        // no-borrow run with no open marks-to-market.
        let mut futures_spec = spec();
        futures_spec.asset_class = AssetClass::Future;
        futures_spec.multiplier = 5.0;
        futures_spec.tick_size = 0.25;
        futures_spec.fees = FeeSpec {
            commission_per_order: 1.0,
            ..FeeSpec::default()
        };
        let mut man = manifest();
        man.universe = vec![futures_spec.clone()];
        // Idealized fills make the entry/exit prices predictable.
        man.execution.bar_fill_mode = BarFillMode::Idealized;

        #[derive(Default)]
        struct EnterThenExitOnSecondBar {
            entered: bool,
            exited: bool,
        }

        impl Strategy for EnterThenExitOnSecondBar {
            fn on_event(&mut self, ctx: &mut StrategyContext, event: &MarketEvent) -> Result<()> {
                if let MarketEvent::Bar(_) = event {
                    if !self.entered {
                        ctx.submit_order(OrderRequest::market(1, OrderSide::Buy, 1.0));
                        self.entered = true;
                    } else if !self.exited {
                        ctx.submit_order(OrderRequest::market(1, OrderSide::Sell, 1.0));
                        self.exited = true;
                    }
                }
                Ok(())
            }
        }

        let events = vec![
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(10),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 20,
                ts_close: 30,
                open: 110.0,
                high: 110.0,
                low: 110.0,
                close: 110.0,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(30),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 40,
                ts_close: 50,
                open: 110.0,
                high: 110.0,
                low: 110.0,
                close: 110.0,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(50),
            }),
        ];
        let mut engine = BacktestEngine::new(EnterThenExitOnSecondBar::default(), man);
        let report = engine.run(events).unwrap();

        // Two fills, position back to flat.
        assert_eq!(report.fills.len(), 2);
        let position = report
            .positions
            .iter()
            .find(|p| p.instrument_id == 1)
            .expect("position recorded");
        assert_eq!(position.qty, 0.0);

        let trade_pnls = realized_trade_pnls(&engine.fills, &engine.instruments);
        let trade_pnl_sum: f64 = trade_pnls.iter().map(|t| t.pnl).sum();
        let portfolio_minus_fees = position.realized_pnl - position.fees;
        let equity_gain = report.metrics.end_equity - report.metrics.start_equity;

        assert!(
            (trade_pnl_sum - portfolio_minus_fees).abs() < 1e-9,
            "engine trade PnL sum {trade_pnl_sum} disagrees with \
             portfolio realized - fees {portfolio_minus_fees}"
        );
        assert!(
            (trade_pnl_sum - equity_gain).abs() < 1e-9,
            "engine trade PnL sum {trade_pnl_sum} disagrees with \
             equity gain {equity_gain}"
        );
    }

    #[test]
    fn realized_trade_pnls_keep_zero_pnl_closed_trades_for_counts() {
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
                price: 100.0,
                qty: 1.0,
                fee: 0.0,
                liquidity: None,
            },
        ];
        let pnls = realized_trade_pnls(&fills, &instruments);
        assert_eq!(pnls, vec![TradePnl { ts: 2, pnl: 0.0 }]);
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
    fn metrics_default_to_unannualized_sharpe() {
        // Default behavior: per-period Sharpe only; annualization is opt-in.
        let events = vec![
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(10),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 10,
                ts_close: 20,
                open: 100.0,
                high: 101.0,
                low: 100.0,
                close: 101.0,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(20),
            }),
        ];
        let mut engine = BacktestEngine::new(NoopStrategy, manifest());
        let report = engine.run(events).unwrap();
        assert!(report.metrics.sharpe_annualization.is_none());
        assert!(report.metrics.annualized_sharpe.is_none());
    }

    #[test]
    fn metrics_annualize_when_factor_is_set() {
        let events = vec![
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(10),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 10,
                ts_close: 20,
                open: 100.0,
                high: 101.0,
                low: 100.0,
                close: 101.0,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(20),
            }),
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 20,
                ts_close: 30,
                open: 101.0,
                high: 102.0,
                low: 100.5,
                close: 101.5,
                volume: 10.0,
                vwap: None,
                feature_cutoff_ts: Some(30),
            }),
        ];
        let mut man = manifest();
        man.metrics.annualization_factor = Some(252.0);
        let mut engine = BacktestEngine::new(NoopStrategy, man);
        let report = engine.run(events).unwrap();
        assert_eq!(report.metrics.sharpe_annualization, Some(252.0));
        let annualized = report.metrics.annualized_sharpe.unwrap();
        let expected = report.metrics.sharpe * 252.0_f64.sqrt();
        assert!(
            (annualized - expected).abs() < 1e-12,
            "annualized {annualized} != per-period {} × sqrt(252)",
            report.metrics.sharpe
        );
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
    fn unsupported_execution_policy_fails_instead_of_falling_back() {
        let mut bad_manifest = manifest();
        bad_manifest.execution.policy = "optimistic".to_string();
        let mut engine = BacktestEngine::new(NoopStrategy, bad_manifest);
        let err = engine.run(Vec::new()).unwrap_err();
        assert!(err.to_string().contains("unsupported execution policy"));
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
        let fillable_volume = 20.0;
        let events = vec![
            MarketEvent::Bar(Bar {
                instrument_id: 1,
                ts_open: 0,
                ts_close: 10,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: fillable_volume,
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
                volume: fillable_volume,
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
                volume: fillable_volume,
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
