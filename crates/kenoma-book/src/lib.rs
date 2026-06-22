//! MBO order-book reconstruction and microstructure validation primitives.

pub mod flow;

use kenoma_types::{fixed_to_price, InstrumentId, MboAction, MboEvent, Side};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

pub const BOOK_DEPTH: usize = 10;
pub const F_LAST: u8 = 0x80;

#[derive(Debug, Clone)]
struct OrderInfo {
    price: i64,
    size: u32,
    side: Side,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CommittedState {
    pub ts: u64,
    pub has_bid: bool,
    pub has_ask: bool,
    pub bids: [[f32; 2]; BOOK_DEPTH],
    pub asks: [[f32; 2]; BOOK_DEPTH],
    pub mid: f32,
    pub spread: f32,
    pub n_bids: u8,
    pub n_asks: u8,
    pub bbo_changed: bool,
}

pub struct BookBuilder {
    instrument_id: InstrumentId,
    orders: FxHashMap<u64, OrderInfo>,
    bid_levels: Vec<(i64, u32)>,
    ask_levels: Vec<(i64, u32)>,
    flow_accums: flow::FlowAccumulators,
    last_flow_state: Option<flow::FlowState>,
    prev_best_bid: Option<i64>,
    prev_best_ask: Option<i64>,
}

fn fixed_to_float(fixed: i64) -> f32 {
    fixed_to_price(fixed) as f32
}

fn level_add(levels: &mut Vec<(i64, u32)>, price: i64, size: u32) {
    match levels.binary_search_by_key(&price, |(p, _)| *p) {
        Ok(idx) => levels[idx].1 += size,
        Err(idx) => levels.insert(idx, (price, size)),
    }
}

fn level_sub(levels: &mut Vec<(i64, u32)>, price: i64, size: u32) {
    if let Ok(idx) = levels.binary_search_by_key(&price, |(p, _)| *p) {
        if levels[idx].1 <= size {
            levels.remove(idx);
        } else {
            levels[idx].1 -= size;
        }
    }
}

fn level_get(levels: &[(i64, u32)], price: i64) -> Option<u32> {
    levels
        .binary_search_by_key(&price, |(p, _)| *p)
        .ok()
        .map(|idx| levels[idx].1)
}

fn snapshot_bids(levels: &[(i64, u32)]) -> ([[f32; 2]; BOOK_DEPTH], u8) {
    let mut out = [[0.0f32; 2]; BOOK_DEPTH];
    let mut count = 0u8;
    for &(price, size) in levels.iter().rev().take(BOOK_DEPTH) {
        out[count as usize] = [fixed_to_float(price), size as f32];
        count += 1;
    }
    (out, count)
}

fn snapshot_asks(levels: &[(i64, u32)]) -> ([[f32; 2]; BOOK_DEPTH], u8) {
    let mut out = [[0.0f32; 2]; BOOK_DEPTH];
    let mut count = 0u8;
    for &(price, size) in levels.iter().take(BOOK_DEPTH) {
        out[count as usize] = [fixed_to_float(price), size as f32];
        count += 1;
    }
    (out, count)
}

impl BookBuilder {
    pub fn new(instrument_id: InstrumentId) -> Self {
        Self {
            instrument_id,
            orders: FxHashMap::default(),
            bid_levels: Vec::with_capacity(64),
            ask_levels: Vec::with_capacity(64),
            flow_accums: flow::FlowAccumulators::with_defaults(),
            last_flow_state: None,
            prev_best_bid: None,
            prev_best_ask: None,
        }
    }

    pub fn process_mbo(&mut self, event: &MboEvent) {
        self.process_event(
            event.ts,
            event.order_id,
            event.instrument_id,
            event.action.as_dbn_char(),
            event.side.as_dbn_char(),
            event.price_fixed,
            event.size,
            event.flags,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process_event(
        &mut self,
        ts_event: u64,
        order_id: u64,
        instrument_id: InstrumentId,
        action: char,
        side: char,
        price: i64,
        size: u32,
        flags: u8,
    ) {
        if instrument_id != self.instrument_id {
            return;
        }

        self.flow_accums.on_event(ts_event, action, side, size);

        let pre_bid = self.bid_levels.last().map(|(p, _)| *p);
        let pre_ask = self.ask_levels.first().map(|(p, _)| *p);

        match MboAction::from_dbn_char(action) {
            MboAction::Add => self.apply_add(order_id, Side::from_dbn_char(side), price, size),
            MboAction::Cancel => self.apply_cancel(order_id),
            MboAction::Modify => {
                self.apply_modify(order_id, Side::from_dbn_char(side), price, size)
            }
            MboAction::Trade => {}
            MboAction::Fill => {}
            MboAction::Clear => self.apply_clear(),
            MboAction::Unknown => {}
        }

        let post_bid = self.bid_levels.last().map(|(p, _)| *p);
        let post_ask = self.ask_levels.first().map(|(p, _)| *p);
        if post_bid != pre_bid || post_ask != pre_ask {
            self.flow_accums.record_bbo_action(action);
        }

        if flags & F_LAST != 0 {
            self.commit(ts_event);
        }
    }

    pub fn best_bid_price(&self) -> Option<i64> {
        self.bid_levels.last().map(|(p, _)| *p)
    }

    pub fn best_ask_price(&self) -> Option<i64> {
        self.ask_levels.first().map(|(p, _)| *p)
    }

    pub fn best_bid_size(&self) -> u32 {
        self.bid_levels.last().map(|(_, s)| *s).unwrap_or(0)
    }

    pub fn best_ask_size(&self) -> u32 {
        self.ask_levels.first().map(|(_, s)| *s).unwrap_or(0)
    }

    pub fn bid_levels_raw(&self) -> &[(i64, u32)] {
        &self.bid_levels
    }

    pub fn ask_levels_raw(&self) -> &[(i64, u32)] {
        &self.ask_levels
    }

    pub fn queue_ahead_at(&self, side: Side, price_fixed: i64) -> u64 {
        let levels = match side {
            Side::Bid => &self.bid_levels,
            Side::Ask => &self.ask_levels,
            Side::None => return 0,
        };
        level_get(levels, price_fixed).unwrap_or(0) as u64
    }

    pub fn current_committed_state(&self, ts: u64) -> CommittedState {
        let has_bid = !self.bid_levels.is_empty();
        let has_ask = !self.ask_levels.is_empty();
        let (bids, n_bids) = snapshot_bids(&self.bid_levels);
        let (asks, n_asks) = snapshot_asks(&self.ask_levels);
        let (mid, spread) = if has_bid && has_ask {
            let best_bid = fixed_to_float(self.bid_levels.last().unwrap().0);
            let best_ask = fixed_to_float(self.ask_levels.first().unwrap().0);
            ((best_bid + best_ask) / 2.0, best_ask - best_bid)
        } else {
            (0.0, 0.0)
        };
        let cur_best_bid = self.bid_levels.last().map(|(p, _)| *p);
        let cur_best_ask = self.ask_levels.first().map(|(p, _)| *p);
        let bbo_changed = cur_best_bid != self.prev_best_bid || cur_best_ask != self.prev_best_ask;
        CommittedState {
            ts,
            has_bid,
            has_ask,
            bids,
            asks,
            mid,
            spread,
            n_bids,
            n_asks,
            bbo_changed,
        }
    }

    pub fn current_flow_state(&self) -> flow::FlowState {
        self.last_flow_state.unwrap_or_default()
    }

    fn levels_for_mut(&mut self, side: Side) -> Option<&mut Vec<(i64, u32)>> {
        match side {
            Side::Bid => Some(&mut self.bid_levels),
            Side::Ask => Some(&mut self.ask_levels),
            Side::None => None,
        }
    }

    fn add_to_level(&mut self, side: Side, price: i64, size: u32) {
        if let Some(levels) = self.levels_for_mut(side) {
            level_add(levels, price, size);
        }
    }

    fn remove_from_level(&mut self, info: &OrderInfo) {
        if let Some(levels) = self.levels_for_mut(info.side) {
            level_sub(levels, info.price, info.size);
        }
    }

    fn apply_add(&mut self, order_id: u64, side: Side, price: i64, size: u32) {
        if side == Side::None {
            return;
        }
        self.orders
            .insert(order_id, OrderInfo { price, size, side });
        self.add_to_level(side, price, size);
    }

    fn apply_cancel(&mut self, order_id: u64) {
        if let Some(info) = self.orders.remove(&order_id) {
            self.remove_from_level(&info);
        }
    }

    fn apply_modify(&mut self, order_id: u64, side: Side, new_price: i64, new_size: u32) {
        if let Some(info) = self.orders.remove(&order_id) {
            self.remove_from_level(&info);
            let updated = OrderInfo {
                price: new_price,
                size: new_size,
                side,
            };
            self.orders.insert(order_id, updated);
            self.add_to_level(side, new_price, new_size);
        }
    }

    fn apply_clear(&mut self) {
        self.orders.clear();
        self.bid_levels.clear();
        self.ask_levels.clear();
    }

    fn commit(&mut self, ts: u64) {
        let cur_best_bid = self.bid_levels.last().map(|(p, _)| *p);
        let cur_best_ask = self.ask_levels.first().map(|(p, _)| *p);
        let bbo_changed = cur_best_bid != self.prev_best_bid || cur_best_ask != self.prev_best_ask;
        self.prev_best_bid = cur_best_bid;
        self.prev_best_ask = cur_best_ask;

        let best_bid_size = cur_best_bid
            .and_then(|p| level_get(&self.bid_levels, p))
            .unwrap_or(0);
        let best_ask_size = cur_best_ask
            .and_then(|p| level_get(&self.ask_levels, p))
            .unwrap_or(0);
        self.last_flow_state = Some(self.flow_accums.snapshot(
            ts,
            bbo_changed,
            cur_best_bid,
            cur_best_ask,
            best_bid_size,
            best_ask_size,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_builder() -> BookBuilder {
        BookBuilder::new(1)
    }

    #[test]
    fn add_order_builds_bbo() {
        let mut bb = make_builder();
        bb.process_event(1000, 100, 1, 'A', 'B', 4_500_000_000_000, 10, 0);
        bb.process_event(1000, 101, 1, 'A', 'A', 4_501_000_000_000, 5, F_LAST);

        assert_eq!(bb.best_bid_price(), Some(4_500_000_000_000));
        assert_eq!(bb.best_ask_price(), Some(4_501_000_000_000));
        assert_eq!(bb.best_bid_size(), 10);
        assert_eq!(bb.best_ask_size(), 5);
    }

    #[test]
    fn cancel_removes_order() {
        let mut bb = make_builder();
        bb.process_event(1000, 100, 1, 'A', 'B', 4_500_000_000_000, 10, F_LAST);
        bb.process_event(2000, 100, 1, 'C', 'B', 4_500_000_000_000, 0, F_LAST);
        assert_eq!(bb.best_bid_price(), None);
    }

    #[test]
    fn modify_updates_price_and_size() {
        let mut bb = make_builder();
        bb.process_event(1000, 100, 1, 'A', 'B', 4_500_000_000_000, 10, F_LAST);
        bb.process_event(2000, 100, 1, 'M', 'B', 4_501_000_000_000, 15, F_LAST);
        assert_eq!(bb.best_bid_price(), Some(4_501_000_000_000));
        assert_eq!(bb.best_bid_size(), 15);
    }

    #[test]
    fn fill_does_not_mutate_book() {
        let mut bb = make_builder();
        bb.process_event(1000, 100, 1, 'A', 'B', 4_500_000_000_000, 10, F_LAST);
        bb.process_event(2000, 100, 1, 'F', 'B', 4_500_000_000_000, 7, F_LAST);
        assert_eq!(bb.best_bid_size(), 10);
    }

    #[test]
    fn instrument_filter_ignores_other_instruments() {
        let mut bb = make_builder();
        bb.process_event(1000, 100, 99, 'A', 'B', 4_500_000_000_000, 10, F_LAST);
        assert_eq!(bb.best_bid_price(), None);
    }

    #[test]
    fn committed_state_has_mid_and_depth() {
        let mut bb = make_builder();
        bb.process_event(1_000_000_000, 100, 1, 'A', 'B', 4_500_000_000_000, 10, 0);
        bb.process_event(
            1_000_000_000,
            101,
            1,
            'A',
            'A',
            4_501_000_000_000,
            5,
            F_LAST,
        );

        let cs = bb.current_committed_state(1_000_000_000);
        assert!(cs.has_bid);
        assert!(cs.has_ask);
        assert!((cs.mid - 4500.5).abs() < 0.01);
        assert_eq!(bb.queue_ahead_at(Side::Bid, 4_500_000_000_000), 10);
    }

    #[test]
    fn flow_tracks_trade() {
        let mut bb = make_builder();
        bb.process_event(1, 100, 1, 'A', 'B', 4_500_000_000_000, 10, 0);
        bb.process_event(1, 101, 1, 'A', 'A', 4_501_000_000_000, 5, F_LAST);
        bb.process_event(2, 0, 1, 'T', 'B', 4_501_000_000_000, 3, F_LAST);
        assert!(bb.current_flow_state().trade_flow[0] > 0.0);
    }
}
