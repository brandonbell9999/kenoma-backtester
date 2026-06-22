//! Backtest statistics and CPCV split generation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts: u64,
    pub equity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradePnl {
    pub ts: u64,
    pub pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub start_equity: f64,
    pub end_equity: f64,
    pub total_return: f64,
    pub max_drawdown: f64,
    /// Per-period Sharpe (mean / stdev of equity-curve returns). NOT
    /// annualized — annualize separately using `annualized_sharpe` with the
    /// correct factor for the equity curve's sampling frequency.
    pub sharpe: f64,
    /// Annualization factor that was applied to `annualized_sharpe`, if any.
    /// `None` means the caller did not request annualization.
    pub sharpe_annualization: Option<f64>,
    /// Annualized Sharpe = `sharpe` × `sqrt(annualization_factor)`. Only
    /// populated when `MetricsConfig.annualization_factor` was set.
    pub annualized_sharpe: Option<f64>,
    pub profit_factor: f64,
    pub trade_count: usize,
}

impl Default for Metrics {
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
            trade_count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Number of equity-curve return periods per year. Optional — when
    /// `None`, only the per-period Sharpe is reported. Setting this is the
    /// caller's contract that the equity curve is regularly sampled at the
    /// implied frequency. Common values: 252 (daily), 252×6.5×60 (1-min RTH),
    /// 252×6.5×3600 (1-sec RTH).
    pub annualization_factor: Option<f64>,
}

pub fn compute_metrics(equity_curve: &[EquityPoint], trade_pnls: &[TradePnl]) -> Metrics {
    compute_metrics_with(equity_curve, trade_pnls, &MetricsConfig::default())
}

pub fn compute_metrics_with(
    equity_curve: &[EquityPoint],
    trade_pnls: &[TradePnl],
    config: &MetricsConfig,
) -> Metrics {
    if equity_curve.is_empty() {
        return Metrics::default();
    }
    let start_equity = equity_curve.first().unwrap().equity;
    let end_equity = equity_curve.last().unwrap().equity;
    let total_return = if start_equity != 0.0 {
        end_equity / start_equity - 1.0
    } else {
        0.0
    };
    let max_drawdown = max_drawdown(equity_curve);
    let returns = equity_returns(equity_curve);
    let sharpe = sharpe_ratio(&returns, 1.0);
    let annualized_sharpe = config
        .annualization_factor
        .map(|factor| sharpe * factor.sqrt());
    let profit_factor = profit_factor(trade_pnls.iter().map(|t| t.pnl));
    Metrics {
        start_equity,
        end_equity,
        total_return,
        max_drawdown,
        sharpe,
        sharpe_annualization: config.annualization_factor,
        annualized_sharpe,
        profit_factor,
        trade_count: trade_pnls.len(),
    }
}

pub fn equity_returns(equity_curve: &[EquityPoint]) -> Vec<f64> {
    equity_curve
        .windows(2)
        .filter_map(|w| {
            let prev = w[0].equity;
            if prev == 0.0 {
                None
            } else {
                Some(w[1].equity / prev - 1.0)
            }
        })
        .collect()
}

pub fn sharpe_ratio(returns: &[f64], annualization: f64) -> f64 {
    if returns.len() < 2 {
        return 0.0;
    }
    let mean = mean(returns);
    let sd = std_dev(returns);
    if sd == 0.0 {
        0.0
    } else {
        mean / sd * annualization
    }
}

pub fn max_drawdown(equity_curve: &[EquityPoint]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut worst = 0.0;
    for point in equity_curve {
        peak = peak.max(point.equity);
        if peak > 0.0 {
            let drawdown = point.equity / peak - 1.0;
            if drawdown < worst {
                worst = drawdown;
            }
        }
    }
    worst
}

pub fn profit_factor<I>(pnls: I) -> f64
where
    I: IntoIterator<Item = f64>,
{
    let mut gains = 0.0;
    let mut losses = 0.0;
    for pnl in pnls {
        if pnl > 0.0 {
            gains += pnl;
        } else {
            losses += pnl.abs();
        }
    }
    if losses > 0.0 {
        gains / losses
    } else if gains > 0.0 {
        f64::INFINITY
    } else {
        0.0
    }
}

pub fn deflated_sharpe_ratio(sharpe: f64, trials: usize) -> f64 {
    if trials <= 1 {
        return sharpe;
    }
    let penalty = (2.0 * (trials as f64).ln()).sqrt();
    sharpe - penalty
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = mean(values);
    let var = values
        .iter()
        .map(|v| {
            let d = v - m;
            d * d
        })
        .sum::<f64>()
        / (values.len() - 1) as f64;
    var.sqrt()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpcvConfig {
    pub n_groups: usize,
    pub k_test: usize,
    pub purge_bars: usize,
    pub embargo_bars: usize,
}

impl Default for CpcvConfig {
    fn default() -> Self {
        Self {
            n_groups: 10,
            k_test: 2,
            purge_bars: 500,
            embargo_bars: 4600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayMeta {
    pub date: i32,
    pub group: usize,
    pub cum_bar_start: usize,
    pub cum_bar_end: usize,
    pub bar_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpcvSplit {
    pub split_idx: usize,
    pub test_groups: Vec<usize>,
    pub train_groups: Vec<usize>,
    pub train_day_indices: Vec<usize>,
    pub test_day_indices: Vec<usize>,
}

pub fn assign_groups(n_days: usize, n_groups: usize) -> Vec<usize> {
    assert!(n_groups > 0, "n_groups must be > 0");
    assert!(n_days >= n_groups, "n_days must be >= n_groups");
    let base_size = n_days / n_groups;
    let remainder = n_days % n_groups;
    let mut groups = Vec::with_capacity(n_days);
    for group in 0..n_groups {
        let size = if group < remainder {
            base_size + 1
        } else {
            base_size
        };
        groups.extend(std::iter::repeat_n(group, size));
    }
    groups
}

pub fn build_day_metas(dates: &[i32], bar_counts: &[usize], n_groups: usize) -> Vec<DayMeta> {
    assert_eq!(dates.len(), bar_counts.len());
    let groups = assign_groups(dates.len(), n_groups);
    let mut cum = 0;
    dates
        .iter()
        .zip(bar_counts)
        .enumerate()
        .map(|(idx, (&date, &bar_count))| {
            let meta = DayMeta {
                date,
                group: groups[idx],
                cum_bar_start: cum,
                cum_bar_end: cum + bar_count,
                bar_count,
            };
            cum += bar_count;
            meta
        })
        .collect()
}

pub fn generate_splits(day_metas: &[DayMeta], config: &CpcvConfig) -> Vec<CpcvSplit> {
    assert!(!day_metas.is_empty(), "day_metas must not be empty");
    assert!(config.n_groups > 0, "n_groups must be > 0");
    assert!(
        config.k_test > 0 && config.k_test <= config.n_groups,
        "k_test must be in 1..=n_groups"
    );
    assert!(
        day_metas.iter().all(|meta| meta.group < config.n_groups),
        "day_metas contain group outside configured range"
    );

    combinations(config.n_groups, config.k_test)
        .into_iter()
        .enumerate()
        .map(|(split_idx, test_groups)| {
            let train_groups = (0..config.n_groups)
                .filter(|group| !test_groups.contains(group))
                .collect::<Vec<_>>();
            let test_day_indices = day_metas
                .iter()
                .enumerate()
                .filter(|(_, meta)| test_groups.contains(&meta.group))
                .map(|(idx, _)| idx)
                .collect::<Vec<_>>();
            let boundaries = find_test_boundaries(day_metas, &test_groups);
            let train_day_indices = day_metas
                .iter()
                .enumerate()
                .filter(|(_, meta)| train_groups.contains(&meta.group))
                .filter(|(_, meta)| !is_purged_or_embargoed(meta, &boundaries, config))
                .map(|(idx, _)| idx)
                .collect::<Vec<_>>();
            CpcvSplit {
                split_idx,
                test_groups,
                train_groups,
                train_day_indices,
                test_day_indices,
            }
        })
        .collect()
}

#[derive(Debug)]
struct TestBoundary {
    block_start: usize,
    block_end: usize,
}

fn find_test_boundaries(day_metas: &[DayMeta], test_groups: &[usize]) -> Vec<TestBoundary> {
    let test_days = day_metas
        .iter()
        .filter(|meta| test_groups.contains(&meta.group))
        .collect::<Vec<_>>();
    if test_days.is_empty() {
        return Vec::new();
    }
    let mut boundaries = Vec::new();
    let mut block_start = test_days[0].cum_bar_start;
    let mut block_end = test_days[0].cum_bar_end;
    for meta in &test_days[1..] {
        if meta.cum_bar_start == block_end {
            block_end = meta.cum_bar_end;
        } else {
            boundaries.push(TestBoundary {
                block_start,
                block_end,
            });
            block_start = meta.cum_bar_start;
            block_end = meta.cum_bar_end;
        }
    }
    boundaries.push(TestBoundary {
        block_start,
        block_end,
    });
    boundaries
}

fn is_purged_or_embargoed(day: &DayMeta, boundaries: &[TestBoundary], config: &CpcvConfig) -> bool {
    for boundary in boundaries {
        let purge_start = boundary.block_start.saturating_sub(config.purge_bars);
        if day.cum_bar_end > purge_start && day.cum_bar_start < boundary.block_start {
            return true;
        }
        let embargo_end = boundary.block_end.saturating_add(config.embargo_bars);
        if day.cum_bar_start < embargo_end && day.cum_bar_end > boundary.block_end {
            return true;
        }
    }
    false
}

fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut result = Vec::new();
    let mut combo = Vec::with_capacity(k);
    combinations_helper(n, k, 0, &mut combo, &mut result);
    result
}

fn combinations_helper(
    n: usize,
    k: usize,
    start: usize,
    current: &mut Vec<usize>,
    result: &mut Vec<Vec<usize>>,
) {
    if current.len() == k {
        result.push(current.clone());
        return;
    }
    for idx in start..n {
        current.push(idx);
        combinations_helper(n, k, idx + 1, current, result);
        current.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawdown_and_profit_factor() {
        let equity = vec![
            EquityPoint {
                ts: 1,
                equity: 100.0,
            },
            EquityPoint {
                ts: 2,
                equity: 90.0,
            },
            EquityPoint {
                ts: 3,
                equity: 120.0,
            },
        ];
        assert!((max_drawdown(&equity) + 0.1).abs() < 1e-12);
        assert_eq!(profit_factor([10.0, -5.0, 5.0]), 3.0);
    }

    #[test]
    fn cpcv_generates_expected_count() {
        let dates = (0..200).collect::<Vec<_>>();
        let bar_counts = vec![100; 200];
        let metas = build_day_metas(&dates, &bar_counts, 10);
        let splits = generate_splits(
            &metas,
            &CpcvConfig {
                purge_bars: 0,
                embargo_bars: 0,
                ..CpcvConfig::default()
            },
        );
        assert_eq!(splits.len(), 45);
        assert_eq!(splits[0].test_groups, vec![0, 1]);
    }

    #[test]
    fn purge_embargo_removes_nearby_train_days() {
        let dates = (0..10).collect::<Vec<_>>();
        let bar_counts = vec![10; 10];
        let metas = build_day_metas(&dates, &bar_counts, 5);
        let splits = generate_splits(
            &metas,
            &CpcvConfig {
                n_groups: 5,
                k_test: 1,
                purge_bars: 10,
                embargo_bars: 10,
            },
        );
        let split = splits.iter().find(|s| s.test_groups == vec![2]).unwrap();
        assert!(split.train_day_indices.len() < 8);
        for idx in &split.train_day_indices {
            assert!(!split.test_day_indices.contains(idx));
        }
    }

    #[test]
    #[should_panic(expected = "k_test must be in 1..=n_groups")]
    fn cpcv_rejects_impossible_test_group_count() {
        let dates = (0..5).collect::<Vec<_>>();
        let bar_counts = vec![10; 5];
        let metas = build_day_metas(&dates, &bar_counts, 5);
        let _ = generate_splits(
            &metas,
            &CpcvConfig {
                n_groups: 5,
                k_test: 6,
                purge_bars: 0,
                embargo_bars: 0,
            },
        );
    }

    #[test]
    fn embargo_uses_saturating_end_boundary() {
        let metas = vec![
            DayMeta {
                date: 0,
                group: 0,
                cum_bar_start: usize::MAX - 3,
                cum_bar_end: usize::MAX - 2,
                bar_count: 1,
            },
            DayMeta {
                date: 1,
                group: 1,
                cum_bar_start: usize::MAX - 2,
                cum_bar_end: usize::MAX - 1,
                bar_count: 1,
            },
        ];
        let splits = generate_splits(
            &metas,
            &CpcvConfig {
                n_groups: 2,
                k_test: 1,
                purge_bars: 0,
                embargo_bars: usize::MAX,
            },
        );
        let split = splits
            .iter()
            .find(|split| split.test_groups == vec![0])
            .unwrap();
        assert!(split.train_day_indices.is_empty());
    }
}
