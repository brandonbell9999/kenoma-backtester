//! Resolver traits for session-aware and rollover-aware backtesting.
//!
//! These traits are the integration surface between the timestamp-ordered
//! backtest engine and external calendar implementations. This crate ships the
//! traits + no-op-equivalent defaults; an external calendars loader
//! provides the production impls against its calendar definitions.
//!
//! When `ExecutionConfig.enable_hg_hooks` is `false` (the default), the
//! engine NEVER consults these resolvers, so user-supplied impls have no
//! observable effect on legacy consumers.

use kenoma_types::{InstrumentId, SessionPhase, TimestampNs};

/// Resolves the trading-session phase for an instrument at a given time.
///
/// The engine calls `session_phase` once per `(instrument_id, event_ts)`
/// when `ExecutionConfig.enable_hg_hooks == true`. When the returned phase
/// differs from the previously cached phase for the same instrument,
/// the engine fires `Strategy::on_session_boundary(ctx, new_phase)` AFTER
/// the strategy's `on_event`/`on_timer` dispatch for the current event has
/// returned and any orders it submitted have been drained.
///
/// Implementations must be pure and side-effect-free; the engine may call
/// `session_phase` many times per second across a backtest's event stream.
pub trait SessionResolver: Send + Sync {
    fn session_phase(&self, ts_ns: TimestampNs, instrument_id: InstrumentId) -> SessionPhase;
}

/// Default `SessionResolver` returning `SessionPhase::Rth` for every input.
///
/// The engine fires `on_session_boundary(Rth)` exactly once per instrument
/// -- on the first event for that instrument -- and then never again,
/// because the phase never changes. For most legacy consumers running with
/// `enable_hg_hooks=true`, this is the desired no-op behaviour: a single
/// boundary fires at startup, after which the strategy is in steady RTH.
///
/// Pods that don't override `on_session_boundary` see this single call
/// hit the trait's default no-op implementation, costing nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysRth;

impl SessionResolver for AlwaysRth {
    fn session_phase(&self, _ts_ns: TimestampNs, _instrument_id: InstrumentId) -> SessionPhase {
        SessionPhase::Rth
    }
}

/// Resolves the active front-month contract for an instrument family.
///
/// `instrument_family` is the symbol root WITHOUT contract code (e.g.,
/// `"ES"`, not `"ESH26"`). The returned string is the active raw contract
/// symbol at `ts_ns`. When the returned symbol differs from the previously
/// cached symbol for the same family, the engine fires
/// `Strategy::on_rollover_boundary(ctx, old, new)`.
///
/// The engine's force-flat behaviour for any open position on `old` runs
/// AFTER the strategy's `on_rollover_boundary` returns and its orders are
/// drained -- this gives the strategy first chance to flatten itself,
/// after which the engine guarantees the position is queued to flat by
/// the next event.
pub trait RolloverResolver: Send + Sync {
    fn active_contract(&self, ts_ns: TimestampNs, instrument_family: &str) -> String;
}

/// Default `RolloverResolver` returning a single fixed contract symbol.
///
/// Constructed with the family-symbol string the engine should report for
/// every query (typically the symbol from the manifest's universe). With
/// `StaticContract`, no rollover boundary ever fires -- `active_contract`
/// returns the same string for every call.
#[derive(Debug, Clone)]
pub struct StaticContract {
    contract: String,
}

impl StaticContract {
    pub fn new(contract: impl Into<String>) -> Self {
        Self {
            contract: contract.into(),
        }
    }
}

impl RolloverResolver for StaticContract {
    fn active_contract(&self, _ts_ns: TimestampNs, _instrument_family: &str) -> String {
        self.contract.clone()
    }
}
