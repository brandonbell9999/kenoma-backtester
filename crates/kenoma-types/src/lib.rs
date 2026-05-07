//! Shared public types for the Kenoma research backtester.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type TimestampNs = u64;
pub type InstrumentId = u32;
pub type OrderId = u64;
pub type Price = f64;
pub type Quantity = f64;
pub type Currency = String;

pub const NANOS_PER_SECOND: f64 = 1_000_000_000.0;
pub const DBN_PRICE_SCALE: f64 = 1_000_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetClass {
    CryptoSpot,
    CryptoPerpetual,
    Future,
    Option,
    Equity,
    Fx,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeSpec {
    #[serde(default)]
    pub commission_per_unit: f64,
    #[serde(default)]
    pub commission_per_order: f64,
    #[serde(default)]
    pub taker_bps: f64,
    #[serde(default)]
    pub maker_bps: f64,
}

impl Default for FeeSpec {
    fn default() -> Self {
        Self {
            commission_per_unit: 0.0,
            commission_per_order: 0.0,
            taker_bps: 0.0,
            maker_bps: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundingSpec {
    pub interval_ns: TimestampNs,
    #[serde(default)]
    pub rate_per_interval: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BorrowSpec {
    #[serde(default)]
    pub annualized_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractSpec {
    pub expiry: Option<String>,
    pub root: Option<String>,
    #[serde(default)]
    pub settlement: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    Call,
    Put,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExerciseStyle {
    European,
    American,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionContract {
    pub underlying_id: InstrumentId,
    pub strike: Price,
    pub expiry: String,
    pub kind: OptionKind,
    #[serde(default = "default_exercise_style")]
    pub exercise: ExerciseStyle,
}

fn default_exercise_style() -> ExerciseStyle {
    ExerciseStyle::European
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentSpec {
    pub id: InstrumentId,
    pub symbol: String,
    pub asset_class: AssetClass,
    pub tick_size: Price,
    pub lot_size: Quantity,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    pub quote_currency: Currency,
    pub base_currency: Option<Currency>,
    pub session_calendar: Option<String>,
    #[serde(default)]
    pub fees: FeeSpec,
    pub funding: Option<FundingSpec>,
    pub borrow: Option<BorrowSpec>,
    pub contract: Option<ContractSpec>,
    pub option: Option<OptionContract>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

fn default_multiplier() -> f64 {
    1.0
}

impl InstrumentSpec {
    pub fn default_cash(
        id: InstrumentId,
        symbol: impl Into<String>,
        quote: impl Into<String>,
    ) -> Self {
        Self {
            id,
            symbol: symbol.into(),
            asset_class: AssetClass::Equity,
            tick_size: 0.01,
            lot_size: 1.0,
            multiplier: 1.0,
            quote_currency: quote.into(),
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

    pub fn round_to_tick(&self, price: Price) -> Price {
        if self.tick_size <= 0.0 {
            return price;
        }
        (price / self.tick_size).round() * self.tick_size
    }

    pub fn min_qty(&self) -> Quantity {
        self.lot_size.max(f64::EPSILON)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Bid,
    Ask,
    None,
}

impl Side {
    pub fn from_dbn_char(ch: char) -> Self {
        match ch {
            'B' => Self::Bid,
            'A' => Self::Ask,
            _ => Self::None,
        }
    }

    pub fn as_dbn_char(self) -> char {
        match self {
            Self::Bid => 'B',
            Self::Ask => 'A',
            Self::None => 'N',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MboAction {
    Add,
    Cancel,
    Modify,
    Trade,
    Fill,
    Clear,
    Unknown,
}

impl MboAction {
    pub fn from_dbn_char(ch: char) -> Self {
        match ch {
            'A' => Self::Add,
            'C' => Self::Cancel,
            'M' => Self::Modify,
            'T' => Self::Trade,
            'F' => Self::Fill,
            'R' => Self::Clear,
            _ => Self::Unknown,
        }
    }

    pub fn as_dbn_char(self) -> char {
        match self {
            Self::Add => 'A',
            Self::Cancel => 'C',
            Self::Modify => 'M',
            Self::Trade => 'T',
            Self::Fill => 'F',
            Self::Clear => 'R',
            Self::Unknown => '?',
        }
    }
}

/// Trading session phase resolved by a `SessionResolver` in `kenoma-engine`.
///
/// `Rth` is regular trading hours, `Eth` is extended trading hours,
/// `Halt` is a market-wide or instrument-level halt. The harness's
/// session-aware behaviour fires `Strategy::on_session_boundary` when
/// the resolved phase changes between successive events for the same
/// instrument. The default resolver `AlwaysRth` returns `Rth` unconditionally,
/// so existing kenoma-backtester consumers never observe a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Rth,
    Eth,
    Halt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub instrument_id: InstrumentId,
    pub ts_open: TimestampNs,
    pub ts_close: TimestampNs,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    #[serde(default)]
    pub volume: Quantity,
    #[serde(default)]
    pub vwap: Option<Price>,
    #[serde(default)]
    pub feature_cutoff_ts: Option<TimestampNs>,
}

impl Bar {
    pub fn timestamp_ns(&self) -> TimestampNs {
        self.ts_close
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub instrument_id: InstrumentId,
    pub ts: TimestampNs,
    pub bid_price: Price,
    pub bid_size: Quantity,
    pub ask_price: Price,
    pub ask_size: Quantity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub instrument_id: InstrumentId,
    pub ts: TimestampNs,
    pub price: Price,
    pub size: Quantity,
    pub aggressor_side: Side,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MboEvent {
    pub instrument_id: InstrumentId,
    pub ts: TimestampNs,
    pub order_id: u64,
    pub action: MboAction,
    pub side: Side,
    pub price_fixed: i64,
    pub size: u32,
    #[serde(default)]
    pub flags: u8,
}

impl MboEvent {
    pub fn price(&self) -> Price {
        fixed_to_price(self.price_fixed)
    }

    pub fn from_dbn_parts(
        ts: TimestampNs,
        order_id: u64,
        instrument_id: InstrumentId,
        action: char,
        side: char,
        price_fixed: i64,
        size: u32,
        flags: u8,
    ) -> Self {
        Self {
            instrument_id,
            ts,
            order_id,
            action: MboAction::from_dbn_char(action),
            side: Side::from_dbn_char(side),
            price_fixed,
            size,
            flags,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerEvent {
    pub ts: TimestampNs,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkEvent {
    pub instrument_id: InstrumentId,
    pub ts: TimestampNs,
    pub price: Price,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum MarketEvent {
    Bar(Bar),
    Quote(Quote),
    Trade(Trade),
    Mbo(MboEvent),
    Timer(TimerEvent),
    Order(OrderUpdate),
    Fill(Fill),
    Mark(MarkEvent),
}

impl MarketEvent {
    pub fn timestamp_ns(&self) -> TimestampNs {
        match self {
            Self::Bar(bar) => bar.ts_close,
            Self::Quote(quote) => quote.ts,
            Self::Trade(trade) => trade.ts,
            Self::Mbo(mbo) => mbo.ts,
            Self::Timer(timer) => timer.ts,
            Self::Order(order) => order.ts,
            Self::Fill(fill) => fill.ts,
            Self::Mark(mark) => mark.ts,
        }
    }

    pub fn instrument_id(&self) -> Option<InstrumentId> {
        match self {
            Self::Bar(bar) => Some(bar.instrument_id),
            Self::Quote(quote) => Some(quote.instrument_id),
            Self::Trade(trade) => Some(trade.instrument_id),
            Self::Mbo(mbo) => Some(mbo.instrument_id),
            Self::Order(order) => Some(order.instrument_id),
            Self::Fill(fill) => Some(fill.instrument_id),
            Self::Mark(mark) => Some(mark.instrument_id),
            Self::Timer(_) => None,
        }
    }

    pub fn priority(&self) -> u8 {
        match self {
            Self::Mbo(_) => 0,
            Self::Quote(_) => 1,
            Self::Trade(_) => 2,
            Self::Bar(_) => 3,
            Self::Mark(_) => 4,
            Self::Timer(_) => 5,
            Self::Order(_) => 6,
            Self::Fill(_) => 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }

    pub fn resting_book_side(self) -> Side {
        match self {
            Self::Buy => Side::Bid,
            Self::Sell => Side::Ask,
        }
    }

    pub fn consuming_book_side(self) -> Side {
        match self {
            Self::Buy => Side::Ask,
            Self::Sell => Side::Bid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit {
        limit_price: Price,
    },
    Stop {
        stop_price: Price,
    },
    StopLimit {
        stop_price: Price,
        limit_price: Price,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Day,
    Gtc,
    Ioc,
    Fok,
}

impl Default for TimeInForce {
    fn default() -> Self {
        Self::Day
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    #[serde(default)]
    pub id: OrderId,
    pub instrument_id: InstrumentId,
    pub side: OrderSide,
    pub qty: Quantity,
    pub order_type: OrderType,
    #[serde(default)]
    pub tif: TimeInForce,
    #[serde(default)]
    pub created_ts: TimestampNs,
    #[serde(default)]
    pub tag: Option<String>,
}

impl OrderRequest {
    pub fn market(instrument_id: InstrumentId, side: OrderSide, qty: Quantity) -> Self {
        Self {
            id: 0,
            instrument_id,
            side,
            qty,
            order_type: OrderType::Market,
            tif: TimeInForce::Day,
            created_ts: 0,
            tag: None,
        }
    }

    pub fn limit(
        instrument_id: InstrumentId,
        side: OrderSide,
        qty: Quantity,
        limit_price: Price,
    ) -> Self {
        Self {
            order_type: OrderType::Limit { limit_price },
            ..Self::market(instrument_id, side, qty)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    New,
    Accepted,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderUpdate {
    pub id: OrderId,
    pub instrument_id: InstrumentId,
    pub ts: TimestampNs,
    pub status: OrderStatus,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub instrument_id: InstrumentId,
    pub ts: TimestampNs,
    pub side: OrderSide,
    pub price: Price,
    pub qty: Quantity,
    #[serde(default)]
    pub fee: f64,
    #[serde(default)]
    pub liquidity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub instrument_id: InstrumentId,
    pub qty: Quantity,
    #[serde(default)]
    pub avg_price: Price,
    #[serde(default)]
    pub realized_pnl: f64,
    #[serde(default)]
    pub fees: f64,
}

impl Position {
    pub fn flat(instrument_id: InstrumentId) -> Self {
        Self {
            instrument_id,
            qty: 0.0,
            avg_price: 0.0,
            realized_pnl: 0.0,
            fees: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts: TimestampNs,
    pub equity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMetrics {
    pub start_equity: f64,
    pub end_equity: f64,
    pub total_return: f64,
    pub max_drawdown: f64,
    /// Per-period Sharpe (NOT annualized).
    pub sharpe: f64,
    /// Annualization factor used to compute `annualized_sharpe`, when set.
    #[serde(default)]
    pub sharpe_annualization: Option<f64>,
    /// Annualized Sharpe (per-period × sqrt(factor)). Only populated when the
    /// run manifest's `metrics.annualization_factor` was set.
    #[serde(default)]
    pub annualized_sharpe: Option<f64>,
    pub profit_factor: f64,
    pub total_fees: f64,
    pub trade_count: usize,
}

impl Default for RunMetrics {
    fn default() -> Self {
        Self {
            start_equity: 0.0,
            end_equity: 0.0,
            total_return: 0.0,
            max_drawdown: 0.0,
            sharpe: 0.0,
            sharpe_annualization: None,
            annualized_sharpe: None,
            profit_factor: 0.0,
            total_fees: 0.0,
            trade_count: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub run_id: String,
    pub metrics: RunMetrics,
    pub orders: Vec<OrderRequest>,
    pub fills: Vec<Fill>,
    pub positions: Vec<Position>,
    pub equity_curve: Vec<EquityPoint>,
}

pub fn fixed_to_price(price_fixed: i64) -> Price {
    price_fixed as f64 / DBN_PRICE_SCALE
}

pub fn price_to_fixed(price: Price) -> i64 {
    (price * DBN_PRICE_SCALE).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_event_sort_keys_are_stable() {
        let bar = MarketEvent::Bar(Bar {
            instrument_id: 1,
            ts_open: 0,
            ts_close: 10,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 0.0,
            vwap: None,
            feature_cutoff_ts: None,
        });
        let quote = MarketEvent::Quote(Quote {
            instrument_id: 1,
            ts: 10,
            bid_price: 0.9,
            bid_size: 1.0,
            ask_price: 1.1,
            ask_size: 1.0,
        });
        assert_eq!(bar.timestamp_ns(), quote.timestamp_ns());
        assert!(quote.priority() < bar.priority());
    }

    #[test]
    fn price_fixed_round_trip() {
        let price = 4500.25;
        assert!((fixed_to_price(price_to_fixed(price)) - price).abs() < 1e-9);
    }
}
