//! Conservative causal fill models and execution-cost primitives.

use kenoma_book::BookBuilder;
use kenoma_types::{
    price_to_fixed, Bar, Fill, InstrumentSpec, MarketEvent, MboAction, MboEvent, OrderRequest,
    OrderSide, OrderType, Price, Quantity, Quote, Side, TimestampNs,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillPolicy {
    ConservativeCausal,
}

impl Default for FillPolicy {
    fn default() -> Self {
        Self::ConservativeCausal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpreadModel {
    Fixed,
    Empirical,
}

impl Default for SpreadModel {
    fn default() -> Self {
        Self::Fixed
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionCosts {
    #[serde(default)]
    pub commission_per_side: f64,
    #[serde(default)]
    pub spread_model: SpreadModel,
    #[serde(default)]
    pub fixed_spread_ticks: f64,
    #[serde(default)]
    pub slippage_ticks: f64,
}

impl Default for ExecutionCosts {
    fn default() -> Self {
        Self {
            commission_per_side: 0.0,
            spread_model: SpreadModel::Fixed,
            fixed_spread_ticks: 1.0,
            slippage_ticks: 0.0,
        }
    }
}

impl ExecutionCosts {
    pub fn per_side_cost(
        &self,
        spec: &InstrumentSpec,
        actual_spread_ticks: f64,
        qty: Quantity,
    ) -> f64 {
        let spread_ticks_used = match self.spread_model {
            SpreadModel::Fixed => self.fixed_spread_ticks,
            SpreadModel::Empirical => actual_spread_ticks,
        };
        let tick_value = spec.tick_size * spec.multiplier;
        let half_spread_cost = (spread_ticks_used / 2.0) * tick_value * qty.abs();
        let slippage_cost = self.slippage_ticks * tick_value * qty.abs();
        self.commission_per_side * qty.abs() + half_spread_cost + slippage_cost
    }

    pub fn order_fee(
        &self,
        spec: &InstrumentSpec,
        qty: Quantity,
        price: Price,
        taker: bool,
    ) -> f64 {
        let notional = qty.abs() * price * spec.multiplier;
        let fee_bps = if taker {
            spec.fees.taker_bps
        } else {
            spec.fees.maker_bps
        };
        self.commission_per_side * qty.abs()
            + spec.fees.commission_per_order
            + spec.fees.commission_per_unit * qty.abs()
            + notional * fee_bps / 10_000.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConservativeCausalFillModel {
    pub policy: FillPolicy,
    pub costs: ExecutionCosts,
}

impl Default for ConservativeCausalFillModel {
    fn default() -> Self {
        Self {
            policy: FillPolicy::ConservativeCausal,
            costs: ExecutionCosts::default(),
        }
    }
}

impl ConservativeCausalFillModel {
    pub fn try_fill(
        &self,
        order: &OrderRequest,
        event: &MarketEvent,
        spec: &InstrumentSpec,
    ) -> Option<Fill> {
        if event.timestamp_ns() <= order.created_ts {
            return None;
        }
        if event.instrument_id() != Some(order.instrument_id) {
            return None;
        }

        match event {
            MarketEvent::Quote(quote) => self.try_fill_quote(order, quote, spec),
            MarketEvent::Bar(bar) => self.try_fill_bar(order, bar, spec),
            MarketEvent::Trade(trade) => self.try_fill_trade(order, trade.ts, trade.price, spec),
            _ => None,
        }
    }

    fn try_fill_quote(
        &self,
        order: &OrderRequest,
        quote: &Quote,
        spec: &InstrumentSpec,
    ) -> Option<Fill> {
        let fill_price = match order.order_type {
            OrderType::Market => match order.side {
                OrderSide::Buy => quote.ask_price,
                OrderSide::Sell => quote.bid_price,
            },
            OrderType::Limit { limit_price } => match order.side {
                OrderSide::Buy if quote.ask_price <= limit_price => {
                    limit_price.min(quote.ask_price)
                }
                OrderSide::Sell if quote.bid_price >= limit_price => {
                    limit_price.max(quote.bid_price)
                }
                _ => return None,
            },
            OrderType::Stop { stop_price } => match order.side {
                OrderSide::Buy if quote.ask_price >= stop_price => quote.ask_price,
                OrderSide::Sell if quote.bid_price <= stop_price => quote.bid_price,
                _ => return None,
            },
            OrderType::StopLimit {
                stop_price,
                limit_price,
            } => match order.side {
                OrderSide::Buy
                    if quote.ask_price >= stop_price && quote.ask_price <= limit_price =>
                {
                    quote.ask_price
                }
                OrderSide::Sell
                    if quote.bid_price <= stop_price && quote.bid_price >= limit_price =>
                {
                    quote.bid_price
                }
                _ => return None,
            },
        };
        self.fill(order, quote.ts, fill_price, spec, true)
    }

    fn try_fill_bar(&self, order: &OrderRequest, bar: &Bar, spec: &InstrumentSpec) -> Option<Fill> {
        if order.created_ts >= bar.ts_open {
            return None;
        }
        let spread = self.costs.fixed_spread_ticks * spec.tick_size;
        let fill_price = match order.order_type {
            OrderType::Market => match order.side {
                OrderSide::Buy => bar.open + spread / 2.0,
                OrderSide::Sell => bar.open - spread / 2.0,
            },
            OrderType::Limit { limit_price } => match order.side {
                OrderSide::Buy if bar.low <= limit_price => limit_price,
                OrderSide::Sell if bar.high >= limit_price => limit_price,
                _ => return None,
            },
            OrderType::Stop { stop_price } => match order.side {
                OrderSide::Buy if bar.high >= stop_price => bar.open.max(stop_price) + spread / 2.0,
                OrderSide::Sell if bar.low <= stop_price => bar.open.min(stop_price) - spread / 2.0,
                _ => return None,
            },
            OrderType::StopLimit {
                stop_price,
                limit_price,
            } => match order.side {
                OrderSide::Buy if bar.open >= stop_price && bar.open <= limit_price => bar.open,
                OrderSide::Sell if bar.open <= stop_price && bar.open >= limit_price => bar.open,
                _ => return None,
            },
        };
        self.fill(order, bar.ts_close, fill_price, spec, true)
    }

    fn try_fill_trade(
        &self,
        order: &OrderRequest,
        ts: TimestampNs,
        trade_price: Price,
        spec: &InstrumentSpec,
    ) -> Option<Fill> {
        let fill_price = match order.order_type {
            OrderType::Market => return None,
            OrderType::Limit { limit_price } => match order.side {
                OrderSide::Buy if trade_price <= limit_price => limit_price,
                OrderSide::Sell if trade_price >= limit_price => limit_price,
                _ => return None,
            },
            OrderType::Stop { stop_price } => match order.side {
                OrderSide::Buy if trade_price >= stop_price => trade_price.max(stop_price),
                OrderSide::Sell if trade_price <= stop_price => trade_price.min(stop_price),
                _ => return None,
            },
            OrderType::StopLimit {
                stop_price,
                limit_price,
            } => match order.side {
                OrderSide::Buy if trade_price >= stop_price && trade_price <= limit_price => {
                    limit_price
                }
                OrderSide::Sell if trade_price <= stop_price && trade_price >= limit_price => {
                    limit_price
                }
                _ => return None,
            },
        };
        self.fill(order, ts, fill_price, spec, true)
    }

    fn fill(
        &self,
        order: &OrderRequest,
        ts: TimestampNs,
        price: Price,
        spec: &InstrumentSpec,
        taker: bool,
    ) -> Option<Fill> {
        let price = conservative_tick_price(spec, order.side, price);
        if violates_limit(order, price) {
            return None;
        }
        Some(Fill {
            order_id: order.id,
            instrument_id: order.instrument_id,
            ts,
            side: order.side,
            price,
            qty: order.qty,
            fee: self.costs.order_fee(spec, order.qty, price, taker),
            liquidity: Some(if taker { "taker" } else { "maker" }.to_string()),
        })
    }
}

fn conservative_tick_price(spec: &InstrumentSpec, side: OrderSide, price: Price) -> Price {
    if spec.tick_size <= 0.0 {
        return price;
    }
    let ticks = price / spec.tick_size;
    let rounded_ticks = match side {
        OrderSide::Buy => (ticks - 1e-12).ceil(),
        OrderSide::Sell => (ticks + 1e-12).floor(),
    };
    rounded_ticks * spec.tick_size
}

fn violates_limit(order: &OrderRequest, fill_price: Price) -> bool {
    match order.order_type {
        OrderType::Limit { limit_price } => match order.side {
            OrderSide::Buy => fill_price > limit_price,
            OrderSide::Sell => fill_price < limit_price,
        },
        OrderType::StopLimit { limit_price, .. } => match order.side {
            OrderSide::Buy => fill_price > limit_price,
            OrderSide::Sell => fill_price < limit_price,
        },
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BracketExit {
    Target { price: Price },
    Stop { price: Price },
}

pub fn pessimistic_bar_bracket_exit(
    entry_side: OrderSide,
    target_price: Price,
    stop_price: Price,
    bar: &Bar,
) -> Option<BracketExit> {
    let target_hit = match entry_side {
        OrderSide::Buy => bar.high >= target_price,
        OrderSide::Sell => bar.low <= target_price,
    };
    let stop_hit = match entry_side {
        OrderSide::Buy => bar.low <= stop_price,
        OrderSide::Sell => bar.high >= stop_price,
    };
    match (target_hit, stop_hit) {
        (true, true) | (false, true) => Some(BracketExit::Stop { price: stop_price }),
        (true, false) => Some(BracketExit::Target {
            price: target_price,
        }),
        (false, false) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MboLimitFillTracker {
    pub order: OrderRequest,
    pub limit_price_fixed: i64,
    pub resting_side: Side,
    pub queue_ahead: Option<u64>,
    pub cumulative_consuming_flow: u64,
    pub filled_qty: Quantity,
    pub filled_at: Option<TimestampNs>,
}

impl MboLimitFillTracker {
    pub fn new(order: OrderRequest) -> Option<Self> {
        let OrderType::Limit { limit_price } = order.order_type else {
            return None;
        };
        Some(Self {
            resting_side: order.side.resting_book_side(),
            limit_price_fixed: price_to_fixed(limit_price),
            order,
            queue_ahead: None,
            cumulative_consuming_flow: 0,
            filled_qty: 0.0,
            filled_at: None,
        })
    }

    pub fn snapshot_queue_ahead(&mut self, builder: &BookBuilder) {
        if self.queue_ahead.is_none() {
            self.queue_ahead =
                Some(builder.queue_ahead_at(self.resting_side, self.limit_price_fixed));
        }
    }

    pub fn observe(&mut self, builder: &BookBuilder, event: &MboEvent) -> bool {
        if self.filled_at.is_some() || event.instrument_id != self.order.instrument_id {
            return self.filled_at.is_some();
        }
        if event.ts < self.order.created_ts {
            return false;
        }
        if self.queue_ahead.is_none() {
            self.queue_ahead =
                Some(builder.queue_ahead_at(self.resting_side, self.limit_price_fixed));
        }
        if event.ts == self.order.created_ts {
            return false;
        }

        let consumes_same_level = event.price_fixed == self.limit_price_fixed
            && event.action == MboAction::Fill
            && event.side == self.resting_side;
        let swept_through = match self.resting_side {
            Side::Bid => event.price_fixed < self.limit_price_fixed,
            Side::Ask => event.price_fixed > self.limit_price_fixed,
            Side::None => false,
        } && matches!(event.action, MboAction::Fill | MboAction::Trade);

        if consumes_same_level {
            self.cumulative_consuming_flow = self
                .cumulative_consuming_flow
                .saturating_add(event.size as u64);
            let consumed_after_queue = self
                .cumulative_consuming_flow
                .saturating_sub(self.queue_ahead.unwrap_or(0));
            self.filled_qty = (consumed_after_queue as f64).min(self.order.qty);
            if self.filled_qty >= self.order.qty {
                self.filled_at = Some(event.ts);
            }
        } else if swept_through {
            self.filled_qty = self.order.qty;
            self.filled_at = Some(event.ts);
        }
        self.filled_at.is_some()
    }

    pub fn to_fill(&self, spec: &InstrumentSpec, costs: &ExecutionCosts) -> Option<Fill> {
        self.filled_at.map(|ts| Fill {
            order_id: self.order.id,
            instrument_id: self.order.instrument_id,
            ts,
            side: self.order.side,
            price: match self.order.order_type {
                OrderType::Limit { limit_price } => limit_price,
                _ => 0.0,
            },
            qty: self.filled_qty,
            fee: costs.order_fee(
                spec,
                self.filled_qty,
                match self.order.order_type {
                    OrderType::Limit { limit_price } => limit_price,
                    _ => 0.0,
                },
                false,
            ),
            liquidity: Some("maker".to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kenoma_types::{AssetClass, FeeSpec, InstrumentSpec, MarkEvent, MarketEvent, Side, Trade};

    fn spec() -> InstrumentSpec {
        InstrumentSpec {
            id: 1,
            symbol: "TEST".to_string(),
            asset_class: AssetClass::Future,
            tick_size: 0.25,
            lot_size: 1.0,
            multiplier: 5.0,
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

    #[test]
    fn market_order_does_not_fill_same_bar() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 1.0);
        order.id = 7;
        order.created_ts = 100;
        let bar = MarketEvent::Bar(Bar {
            instrument_id: 1,
            ts_open: 0,
            ts_close: 100,
            open: 10.0,
            high: 11.0,
            low: 9.0,
            close: 10.5,
            volume: 10.0,
            vwap: None,
            feature_cutoff_ts: None,
        });
        assert!(fill_model.try_fill(&order, &bar, &spec()).is_none());
    }

    #[test]
    fn market_order_fills_next_quote_at_ask() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 2.0);
        order.id = 7;
        order.created_ts = 100;
        let event = MarketEvent::Quote(Quote {
            instrument_id: 1,
            ts: 101,
            bid_price: 9.9,
            bid_size: 1.0,
            ask_price: 10.1,
            ask_size: 1.0,
        });
        let fill = fill_model.try_fill(&order, &event, &spec()).unwrap();
        assert_eq!(fill.price, 10.25);
        assert_eq!(fill.side, OrderSide::Buy);
    }

    #[test]
    fn conservative_tick_rounding_never_improves_market_fills() {
        let fill_model = ConservativeCausalFillModel::default();
        let event = MarketEvent::Quote(Quote {
            instrument_id: 1,
            ts: 101,
            bid_price: 9.9,
            bid_size: 1.0,
            ask_price: 10.1,
            ask_size: 1.0,
        });

        let mut buy = OrderRequest::market(1, OrderSide::Buy, 1.0);
        buy.created_ts = 100;
        let buy_fill = fill_model.try_fill(&buy, &event, &spec()).unwrap();
        assert!(
            buy_fill.price >= 10.1,
            "buy fill improved below observed ask: {}",
            buy_fill.price
        );

        let mut sell = OrderRequest::market(1, OrderSide::Sell, 1.0);
        sell.created_ts = 100;
        let sell_fill = fill_model.try_fill(&sell, &event, &spec()).unwrap();
        assert!(
            sell_fill.price <= 9.9,
            "sell fill improved above observed bid: {}",
            sell_fill.price
        );
    }

    #[test]
    fn conservative_tick_rounding_can_block_mispriced_limit_fill() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::limit(1, OrderSide::Buy, 1.0, 10.24);
        order.created_ts = 100;
        let event = MarketEvent::Quote(Quote {
            instrument_id: 1,
            ts: 101,
            bid_price: 9.75,
            bid_size: 1.0,
            ask_price: 10.23,
            ask_size: 1.0,
        });
        assert!(fill_model.try_fill(&order, &event, &spec()).is_none());
    }

    #[test]
    fn mark_event_is_not_executable_liquidity() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 1.0);
        order.created_ts = 100;
        let event = MarketEvent::Mark(MarkEvent {
            instrument_id: 1,
            ts: 101,
            price: 10.0,
        });
        assert!(fill_model.try_fill(&order, &event, &spec()).is_none());
    }

    #[test]
    fn bar_does_not_fill_order_created_after_bar_open() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::limit(1, OrderSide::Buy, 1.0, 9.0);
        order.created_ts = 150;
        let event = MarketEvent::Bar(Bar {
            instrument_id: 1,
            ts_open: 100,
            ts_close: 200,
            open: 10.0,
            high: 10.0,
            low: 8.0,
            close: 9.5,
            volume: 1.0,
            vwap: None,
            feature_cutoff_ts: None,
        });
        assert!(fill_model.try_fill(&order, &event, &spec()).is_none());
    }

    #[test]
    fn bar_does_not_fill_order_created_at_bar_open() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 1.0);
        order.created_ts = 100;
        let event = MarketEvent::Bar(Bar {
            instrument_id: 1,
            ts_open: 100,
            ts_close: 200,
            open: 10.0,
            high: 10.0,
            low: 10.0,
            close: 10.0,
            volume: 1.0,
            vwap: None,
            feature_cutoff_ts: None,
        });
        assert!(fill_model.try_fill(&order, &event, &spec()).is_none());
    }

    #[test]
    fn market_order_does_not_fill_from_trade_print() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 10.0);
        order.created_ts = 1;
        let event = MarketEvent::Trade(Trade {
            instrument_id: 1,
            ts: 2,
            price: 99.0,
            size: 1.0,
            aggressor_side: Side::Bid,
        });
        assert!(fill_model.try_fill(&order, &event, &spec()).is_none());
    }

    #[test]
    fn buy_stop_gap_fills_at_trade_price_not_stop_price() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 1.0);
        order.order_type = OrderType::Stop { stop_price: 100.0 };
        order.created_ts = 1;
        let event = MarketEvent::Trade(Trade {
            instrument_id: 1,
            ts: 2,
            price: 105.0,
            size: 1.0,
            aggressor_side: Side::Ask,
        });
        let fill = fill_model.try_fill(&order, &event, &spec()).unwrap();
        assert_eq!(fill.price, 105.0);
    }

    #[test]
    fn buy_stop_gap_on_bar_fills_at_open_or_worse() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 1.0);
        order.order_type = OrderType::Stop { stop_price: 100.0 };
        order.created_ts = 1;
        let event = MarketEvent::Bar(Bar {
            instrument_id: 1,
            ts_open: 2,
            ts_close: 3,
            open: 120.0,
            high: 121.0,
            low: 119.0,
            close: 120.0,
            volume: 1.0,
            vwap: None,
            feature_cutoff_ts: None,
        });
        let fill = fill_model.try_fill(&order, &event, &spec()).unwrap();
        assert_eq!(fill.price, 120.25);
    }

    #[test]
    fn ambiguous_bar_stop_limit_does_not_fill_intrabar() {
        let fill_model = ConservativeCausalFillModel::default();
        let mut order = OrderRequest::market(1, OrderSide::Buy, 1.0);
        order.order_type = OrderType::StopLimit {
            stop_price: 100.0,
            limit_price: 101.0,
        };
        order.created_ts = 1;
        let event = MarketEvent::Bar(Bar {
            instrument_id: 1,
            ts_open: 2,
            ts_close: 3,
            open: 99.0,
            high: 102.0,
            low: 98.0,
            close: 100.0,
            volume: 1.0,
            vwap: None,
            feature_cutoff_ts: None,
        });
        assert!(fill_model.try_fill(&order, &event, &spec()).is_none());
    }

    #[test]
    fn pessimistic_bar_prefers_stop_on_ambiguity() {
        let bar = Bar {
            instrument_id: 1,
            ts_open: 0,
            ts_close: 10,
            open: 100.0,
            high: 110.0,
            low: 90.0,
            close: 100.0,
            volume: 1.0,
            vwap: None,
            feature_cutoff_ts: None,
        };
        let exit = pessimistic_bar_bracket_exit(OrderSide::Buy, 108.0, 95.0, &bar);
        assert_eq!(exit, Some(BracketExit::Stop { price: 95.0 }));
    }

    #[test]
    fn mbo_limit_requires_flow_through_queue_ahead() {
        let mut builder = BookBuilder::new(1);
        builder.process_event(1, 10, 1, 'A', 'B', 100_000_000_000, 5, kenoma_book::F_LAST);
        let mut order = OrderRequest::limit(1, OrderSide::Buy, 1.0, 100.0);
        order.id = 99;
        order.created_ts = 1;
        let mut tracker = MboLimitFillTracker::new(order).unwrap();
        let first = MboEvent::from_dbn_parts(2, 10, 1, 'F', 'B', 100_000_000_000, 5, 0);
        assert!(!tracker.observe(&builder, &first));
        let second = MboEvent::from_dbn_parts(3, 11, 1, 'F', 'B', 100_000_000_000, 1, 0);
        assert!(tracker.observe(&builder, &second));
        assert_eq!(tracker.filled_at, Some(3));
        assert_eq!(tracker.filled_qty, 1.0);
    }

    #[test]
    fn mbo_cancels_ahead_do_not_improve_queue_position() {
        let mut builder = BookBuilder::new(1);
        builder.process_event(1, 10, 1, 'A', 'B', 100_000_000_000, 10, kenoma_book::F_LAST);
        let mut order = OrderRequest::limit(1, OrderSide::Buy, 1.0, 100.0);
        order.id = 99;
        order.created_ts = 1;
        let mut tracker = MboLimitFillTracker::new(order).unwrap();
        tracker.snapshot_queue_ahead(&builder);

        let cancel = MboEvent::from_dbn_parts(2, 10, 1, 'C', 'B', 100_000_000_000, 10, 0);
        builder.process_mbo(&cancel);
        assert!(!tracker.observe(&builder, &cancel));
        assert_eq!(tracker.queue_ahead, Some(10));

        let first_fill = MboEvent::from_dbn_parts(3, 11, 1, 'F', 'B', 100_000_000_000, 10, 0);
        assert!(!tracker.observe(&builder, &first_fill));
        let second_fill = MboEvent::from_dbn_parts(4, 12, 1, 'F', 'B', 100_000_000_000, 1, 0);
        assert!(tracker.observe(&builder, &second_fill));
    }

    #[test]
    fn mbo_limit_does_not_full_fill_on_one_unit_past_queue() {
        let mut builder = BookBuilder::new(1);
        builder.process_event(1, 10, 1, 'A', 'B', 100_000_000_000, 10, kenoma_book::F_LAST);
        let mut order = OrderRequest::limit(1, OrderSide::Buy, 5.0, 100.0);
        order.id = 99;
        order.created_ts = 1;
        let mut tracker = MboLimitFillTracker::new(order).unwrap();
        tracker.snapshot_queue_ahead(&builder);

        let first_fill = MboEvent::from_dbn_parts(2, 11, 1, 'F', 'B', 100_000_000_000, 11, 0);
        assert!(!tracker.observe(&builder, &first_fill));
        assert_eq!(tracker.filled_qty, 1.0);
        assert_eq!(tracker.filled_at, None);
        assert_eq!(
            tracker.to_fill(&spec(), &ExecutionCosts::default()),
            None,
            "partial queue penetration must not be reported as a completed full fill"
        );

        let completion = MboEvent::from_dbn_parts(3, 12, 1, 'F', 'B', 100_000_000_000, 4, 0);
        assert!(tracker.observe(&builder, &completion));
        let fill = tracker
            .to_fill(&spec(), &ExecutionCosts::default())
            .unwrap();
        assert_eq!(fill.qty, 5.0);
    }
}
