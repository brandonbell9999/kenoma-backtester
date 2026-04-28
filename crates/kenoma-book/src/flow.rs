//! Event-count flow accumulators for MBO replay.

use serde::{Deserialize, Serialize};

pub const NUM_SCALES: usize = 3;
pub const DEFAULT_HALFLIVES_EVENTS: [f64; NUM_SCALES] = [50.0, 500.0, 5000.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum BboChangeCause {
    None = 0,
    AggressiveTrade = 1,
    Cancel = 2,
    NewLevel = 3,
    Modify = 4,
    Multiple = 5,
}

#[derive(Debug, Clone)]
pub struct EmaAccumulator {
    values: [f64; NUM_SCALES],
    decays: [f64; NUM_SCALES],
}

impl EmaAccumulator {
    pub fn new(halflives_events: [f64; NUM_SCALES]) -> Self {
        let mut decays = [0.0; NUM_SCALES];
        for i in 0..NUM_SCALES {
            decays[i] = (-std::f64::consts::LN_2 / halflives_events[i]).exp();
        }
        Self {
            values: [0.0; NUM_SCALES],
            decays,
        }
    }

    #[inline]
    pub fn update(&mut self, value: f64) {
        for i in 0..NUM_SCALES {
            self.values[i] = self.values[i] * self.decays[i] + value;
        }
    }

    #[inline]
    pub fn query(&self) -> [f64; NUM_SCALES] {
        self.values
    }
}

#[derive(Debug, Clone)]
pub struct FlowAccumulators {
    pub trade_flow: EmaAccumulator,
    pub cancel_bid: EmaAccumulator,
    pub cancel_ask: EmaAccumulator,
    pub add_bid: EmaAccumulator,
    pub add_ask: EmaAccumulator,
    pub event_count: EmaAccumulator,
    pub trade_count: EmaAccumulator,
    pub ofi: EmaAccumulator,
    prev_best_bid_size: u32,
    prev_best_ask_size: u32,
    prev_best_bid_price: Option<i64>,
    prev_best_ask_price: Option<i64>,
    pending_bbo_action_mask: u8,
    last_event_ts: u64,
    events_initialized: bool,
    last_inter_event_ns: f64,
}

impl FlowAccumulators {
    pub fn new(halflives_events: [f64; NUM_SCALES]) -> Self {
        Self {
            trade_flow: EmaAccumulator::new(halflives_events),
            cancel_bid: EmaAccumulator::new(halflives_events),
            cancel_ask: EmaAccumulator::new(halflives_events),
            add_bid: EmaAccumulator::new(halflives_events),
            add_ask: EmaAccumulator::new(halflives_events),
            event_count: EmaAccumulator::new(halflives_events),
            trade_count: EmaAccumulator::new(halflives_events),
            ofi: EmaAccumulator::new(halflives_events),
            prev_best_bid_size: 0,
            prev_best_ask_size: 0,
            prev_best_bid_price: None,
            prev_best_ask_price: None,
            pending_bbo_action_mask: 0,
            last_event_ts: 0,
            events_initialized: false,
            last_inter_event_ns: 0.0,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_HALFLIVES_EVENTS)
    }

    pub fn on_event(&mut self, ts: u64, action: char, side: char, size: u32) {
        if self.events_initialized && ts >= self.last_event_ts {
            self.last_inter_event_ns = (ts - self.last_event_ts) as f64;
        }
        self.last_event_ts = ts;
        self.events_initialized = true;

        let sz = size as f64;
        self.event_count.update(1.0);

        match action {
            'T' => {
                self.trade_flow.update(if side == 'B' { sz } else { -sz });
                self.trade_count.update(1.0);
            }
            'A' => {
                if side == 'B' {
                    self.add_bid.update(sz);
                } else {
                    self.add_ask.update(sz);
                }
            }
            'C' => {
                if side == 'B' {
                    self.cancel_bid.update(sz);
                } else {
                    self.cancel_ask.update(sz);
                }
            }
            'F' => self.trade_flow.update(if side == 'B' { sz } else { -sz }),
            _ => {}
        }
    }

    pub fn record_bbo_action(&mut self, action: char) {
        self.pending_bbo_action_mask |= match action {
            'T' | 'F' => 0x01,
            'C' => 0x02,
            'A' => 0x04,
            'M' => 0x08,
            _ => 0,
        };
    }

    pub fn snapshot(
        &mut self,
        ts: u64,
        bbo_changed: bool,
        best_bid_price: Option<i64>,
        best_ask_price: Option<i64>,
        best_bid_size: u32,
        best_ask_size: u32,
    ) -> FlowState {
        let bid_ofi = compute_side_ofi(
            self.prev_best_bid_price,
            self.prev_best_bid_size,
            best_bid_price,
            best_bid_size,
        );
        let ask_ofi = compute_side_ofi(
            self.prev_best_ask_price,
            self.prev_best_ask_size,
            best_ask_price,
            best_ask_size,
        );
        let ofi_value = bid_ofi - ask_ofi;
        if ofi_value != 0.0 {
            self.ofi.update(ofi_value);
        }

        self.prev_best_bid_price = best_bid_price;
        self.prev_best_ask_price = best_ask_price;
        self.prev_best_bid_size = best_bid_size;
        self.prev_best_ask_size = best_ask_size;

        let cause = if bbo_changed {
            classify_bbo_cause(self.pending_bbo_action_mask)
        } else {
            BboChangeCause::None
        };
        self.pending_bbo_action_mask = 0;

        let inter_event_time_ns = self.last_inter_event_ns as f32;
        let event_rate = if self.last_inter_event_ns > 0.0 {
            (1e9 / self.last_inter_event_ns) as f32
        } else {
            0.0
        };

        FlowState {
            ts,
            trade_flow: to_f32_3(self.trade_flow.query()),
            cancel_bid: to_f32_3(self.cancel_bid.query()),
            cancel_ask: to_f32_3(self.cancel_ask.query()),
            add_bid: to_f32_3(self.add_bid.query()),
            add_ask: to_f32_3(self.add_ask.query()),
            event_intensity: to_f32_3(self.event_count.query()),
            trade_intensity: to_f32_3(self.trade_count.query()),
            ofi: to_f32_3(self.ofi.query()),
            inter_event_time_ns,
            event_rate,
            bbo_change_cause: cause,
        }
    }
}

fn compute_side_ofi(
    prev_price: Option<i64>,
    prev_size: u32,
    cur_price: Option<i64>,
    cur_size: u32,
) -> f64 {
    match (prev_price, cur_price) {
        (Some(prev), Some(cur)) if cur > prev => cur_size as f64,
        (Some(prev), Some(cur)) if cur == prev => cur_size as f64 - prev_size as f64,
        (Some(_), Some(_)) => -(prev_size as f64),
        (None, Some(_)) => cur_size as f64,
        (Some(_), None) => -(prev_size as f64),
        (None, None) => 0.0,
    }
}

fn classify_bbo_cause(mask: u8) -> BboChangeCause {
    if mask == 0 {
        return BboChangeCause::None;
    }
    if mask.count_ones() > 1 {
        return BboChangeCause::Multiple;
    }
    match mask {
        0x01 => BboChangeCause::AggressiveTrade,
        0x02 => BboChangeCause::Cancel,
        0x04 => BboChangeCause::NewLevel,
        0x08 => BboChangeCause::Modify,
        _ => BboChangeCause::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FlowState {
    pub ts: u64,
    pub trade_flow: [f32; NUM_SCALES],
    pub cancel_bid: [f32; NUM_SCALES],
    pub cancel_ask: [f32; NUM_SCALES],
    pub add_bid: [f32; NUM_SCALES],
    pub add_ask: [f32; NUM_SCALES],
    pub event_intensity: [f32; NUM_SCALES],
    pub trade_intensity: [f32; NUM_SCALES],
    pub ofi: [f32; NUM_SCALES],
    pub inter_event_time_ns: f32,
    pub event_rate: f32,
    pub bbo_change_cause: BboChangeCause,
}

impl Default for FlowState {
    fn default() -> Self {
        Self {
            ts: 0,
            trade_flow: [0.0; NUM_SCALES],
            cancel_bid: [0.0; NUM_SCALES],
            cancel_ask: [0.0; NUM_SCALES],
            add_bid: [0.0; NUM_SCALES],
            add_ask: [0.0; NUM_SCALES],
            event_intensity: [0.0; NUM_SCALES],
            trade_intensity: [0.0; NUM_SCALES],
            ofi: [0.0; NUM_SCALES],
            inter_event_time_ns: 0.0,
            event_rate: 0.0,
            bbo_change_cause: BboChangeCause::None,
        }
    }
}

pub const FLOW_FEATURE_NAMES: &[&str] = &[
    "trade_flow_fast",
    "trade_flow_med",
    "trade_flow_slow",
    "cancel_bid_fast",
    "cancel_bid_med",
    "cancel_bid_slow",
    "cancel_ask_fast",
    "cancel_ask_med",
    "cancel_ask_slow",
    "add_bid_fast",
    "add_bid_med",
    "add_bid_slow",
    "add_ask_fast",
    "add_ask_med",
    "add_ask_slow",
    "event_intensity_fast",
    "event_intensity_med",
    "event_intensity_slow",
    "trade_intensity_fast",
    "trade_intensity_med",
    "trade_intensity_slow",
    "ofi_fast",
    "ofi_med",
    "ofi_slow",
    "inter_event_time_ns",
    "event_rate",
    "bbo_change_cause",
];

pub const NUM_FLOW_FEATURES: usize = 27;

impl FlowState {
    pub fn to_features(&self) -> [f32; NUM_FLOW_FEATURES] {
        let mut out = [0.0; NUM_FLOW_FEATURES];
        let mut i = 0;
        for values in [
            &self.trade_flow,
            &self.cancel_bid,
            &self.cancel_ask,
            &self.add_bid,
            &self.add_ask,
            &self.event_intensity,
            &self.trade_intensity,
            &self.ofi,
        ] {
            for &value in values {
                out[i] = value;
                i += 1;
            }
        }
        out[i] = self.inter_event_time_ns;
        i += 1;
        out[i] = self.event_rate;
        i += 1;
        out[i] = self.bbo_change_cause as u8 as f32;
        out
    }
}

#[inline]
fn to_f32_3(v: [f64; NUM_SCALES]) -> [f32; NUM_SCALES] {
    [v[0] as f32, v[1] as f32, v[2] as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_decays_by_halflife() {
        let mut ema = EmaAccumulator::new(DEFAULT_HALFLIVES_EVENTS);
        ema.update(100.0);
        for _ in 0..50 {
            ema.update(0.0);
        }
        assert!((ema.query()[0] - 50.0).abs() < 1.0);
        assert!(ema.query()[2] > 90.0);
    }

    #[test]
    fn flow_signed_trade() {
        let mut accums = FlowAccumulators::with_defaults();
        accums.on_event(1, 'T', 'B', 5);
        accums.on_event(2, 'T', 'A', 3);
        let state = accums.snapshot(2, false, Some(100), Some(101), 10, 10);
        assert!(state.trade_flow[0] > 1.0);
        assert!(state.trade_intensity[0] > 1.0);
    }

    #[test]
    fn bbo_cause_multiple() {
        let mut accums = FlowAccumulators::with_defaults();
        accums.record_bbo_action('C');
        accums.record_bbo_action('A');
        let state = accums.snapshot(1, true, Some(100), Some(101), 10, 10);
        assert_eq!(state.bbo_change_cause, BboChangeCause::Multiple);
    }

    #[test]
    fn ofi_queue_growth() {
        let mut accums = FlowAccumulators::with_defaults();
        accums.snapshot(0, false, Some(100), Some(102), 10, 10);
        let state = accums.snapshot(1, false, Some(100), Some(102), 15, 10);
        assert!((state.ofi[0] - 5.0).abs() < 0.1);
    }
}
