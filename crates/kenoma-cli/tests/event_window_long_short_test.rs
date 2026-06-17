// Drives the strategy through the real engine on synthetic daily bars:
// 2 basket names (ids 1,2) + 1 ETF (id 3). Entry signal between bar0 and bar1.
// Asserts: 3 entry fills (2 buys + 1 sell), 3 exit fills (flat at end), and the
// long-short total_return is finite.
//
// NOTE on bar count: the engine fills conservatively on the *next* bar after an
// order is submitted (`evaluate_pending_orders` runs at the start of each event;
// `try_fill_bar` requires `order.created_ts < bar.ts_open`). Orders submitted on
// the final bar for an instrument therefore never see a subsequent bar to fill
// against. The strategy submits its flatten on the first bar with
// `ts_open >= exit_signal_ts` (here day3), so we need a trailing day4 bar for the
// exit to fill at day4's open. Five trading days (0..5) gives: entry submitted
// day1 -> filled day2 open; exit submitted day3 -> filled day4 open.
use kenoma_cli::{run_in_memory_for_test, EventWindowLongShort};
use kenoma_types::{Bar, MarketEvent};

fn bar(id: u32, day: u64, o: f64, c: f64) -> MarketEvent {
    let ts_open = day * 86_400_000_000_000;
    MarketEvent::Bar(Bar {
        instrument_id: id,
        ts_open,
        ts_close: ts_open + 23_400_000_000_000,
        open: o,
        high: o.max(c) + 1.0,
        low: o.min(c) - 1.0,
        close: c,
        volume: 1000.0,
        vwap: None,
        feature_cutoff_ts: Some(ts_open + 23_400_000_000_000),
    })
}

#[test]
fn enters_long_basket_short_etf_then_flattens() {
    // 5 trading days. entry_signal between day0 close and day1 open -> act on
    // day1, fill day2 open. exit_signal at day3 open -> act on day3, fill day4 open.
    let entry_signal_ts = 86_400_000_000_000 - 1; // just before day1 open
    let exit_signal_ts = 3 * 86_400_000_000_000; // day3 open
    let mut events = vec![];
    for day in 0..5u64 {
        // basket names rise 0%/+? over the hold; etf rises modestly
        events.push(bar(1, day, 100.0, 100.0)); // name1 flat
        events.push(bar(2, day, 100.0, 100.0 + day as f64 * 5.0)); // name2 rising
        events.push(bar(3, day, 50.0, 50.0 + day as f64 * 1.0)); // etf rising
    }
    let strat = EventWindowLongShort::from_parts(
        vec![1, 2],
        3,
        1.0,
        10_000.0,
        entry_signal_ts,
        exit_signal_ts,
    );
    let report = run_in_memory_for_test(strat, events, 10_000.0);
    // 3 entries + 3 exits
    assert_eq!(report.fills.len(), 6, "expected 3 entry + 3 exit fills");
    // ends flat: net qty per instrument ~ 0 in final positions
    for p in &report.positions {
        assert!(
            p.qty.abs() < 1e-9,
            "instrument {} not flat",
            p.instrument_id
        );
    }
    // long-short return is finite
    assert!(report.metrics.total_return.is_finite());
}
