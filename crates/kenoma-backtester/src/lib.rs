//! Facade crate for the Kenoma research backtester.

pub use kenoma_audit as audit;
pub use kenoma_book as book;
pub use kenoma_data as data;
pub use kenoma_engine as engine;
pub use kenoma_execution as execution;
pub use kenoma_options as options;
pub use kenoma_portfolio as portfolio;
pub use kenoma_stats as stats;
pub use kenoma_types as types;

pub use audit::{AuditTrail, ValidationMode};
pub use engine::{BacktestEngine, RunManifest, Strategy, StrategyContext};
pub use types::{
    Bar, Fill, InstrumentSpec, MarketEvent, OrderRequest, OrderSide, OrderType, Position, Quote,
    RunReport, Trade,
};
