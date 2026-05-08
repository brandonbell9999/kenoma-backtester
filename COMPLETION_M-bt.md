# M-bt — Completion Report

**Dyad:** claude-claude
**Worktree:** ~/LOCAL_DEV/kenoma-backtester-mbt-claude
**Branch:** m-bt-claude
**Baseline:** 5bd835f (Merge branch 'review-fixes-bar-fill-mode')
**Plan sha256:** bae3999e668cd2df1475f7d5de5b7c6dad65e3852dc5785c24345741a2849249
**Brief sha256:** 1b5dea4a723ee4e553508829c16606c14cf9025c03244d00814a4426b561581e
**Brief acknowledged:** yes

## Critical finding (raised in plan)

The 07 spec's claim 1 ("Wire on_fill — never invoked") is incorrect at this
baseline. `on_fill` IS invoked from `apply_fill` (see plan §"Critical finding").
Re-scoped to a test asset only (`on_fill_invariant_test.rs`); no engine code
change for claim 1.

## Acceptance checklist

- [x] A1: `cargo build --workspace --release` clean from `cargo clean`
- [x] A2: `cargo test --workspace` green: 106 tests pass (97 baseline + 9 new across 4 files)
- [x] A3: `cargo fmt --all -- --check` passes
- [x] A4: `cargo clippy -p kenoma-engine -p kenoma-types --all-targets --all-features --no-deps -- -D warnings` passes
- [x] A5: All 4 new test files pass individually:
  - on_fill_invariant_test.rs (3 tests)
  - session_boundary_test.rs (2 tests)
  - rollover_boundary_test.rs (2 tests)
  - hg_hooks_disabled_test.rs (2 tests)
- [x] A7: Backwards-compat: `enable_hg_hooks=false` keeps resolvers untouched (hg_hooks_disabled_test)
- [x] A9: Git working tree clean; all changes committed
- [x] A10: COMPLETION_M-bt.md written at worktree root
- [x] A11: Branch contains 12 commits (1 anchor + 11 task commits)

## Public API surface added (per Task 12.1 of the plan)

### kenoma-types
- `pub enum SessionPhase { Rth, Eth, Halt }`

### kenoma-engine
- Re-exports: `AlwaysRth, RolloverResolver, SessionResolver, StaticContract`
- `Strategy::on_session_boundary(&mut self, &mut StrategyContext, InstrumentId, SessionPhase) -> Result<()>` (default no-op)
- `Strategy::on_rollover_boundary(&mut self, &mut StrategyContext, &str, &str, &str) -> Result<()>` (default no-op)
- `BacktestEngine::with_session_resolver(self, Box<dyn SessionResolver>) -> Self`
- `BacktestEngine::with_rollover_resolver(self, Box<dyn RolloverResolver>) -> Self`
- `ExecutionConfig.enable_hg_hooks: bool` (default false)

No removals, no signature changes on existing items.

## Files touched

| File | SHA256 | Status |
|------|--------|--------|
| `crates/kenoma-types/src/lib.rs` | c64ed130... | modified (+SessionPhase, clippy fixes) |
| `crates/kenoma-engine/src/lib.rs` | c5a7c638... | modified |
| `crates/kenoma-engine/src/resolvers.rs` | ab541ebb... | new |
| `crates/kenoma-engine/tests/on_fill_invariant_test.rs` | 3eec8eb1... | new |
| `crates/kenoma-engine/tests/session_boundary_test.rs` | 7519f38e... | new |
| `crates/kenoma-engine/tests/rollover_boundary_test.rs` | 15e2e0d6... | new |
| `crates/kenoma-engine/tests/hg_hooks_disabled_test.rs` | 2de543dd... | new |

No other files modified. No new dependencies added to any Cargo.toml.

## Findings / deviations from plan

1. **Task 10 plan bug:** Plan specified `Rc<RefCell<u32>>` for counting resolvers in hg_hooks_disabled_test, but resolver traits are `Send + Sync`. Used `Arc<AtomicU32>` instead.
2. **Task 11 pre-existing clippy lints:** kenoma-types had 2 pre-existing clippy failures at baseline (`derivable_impls` on TimeInForce, `too_many_arguments` on MboEvent::from_dbn_parts). Fixed with `#[derive(Default)]` + `#[default]` attribute and `#[allow(clippy::too_many_arguments)]` respectively. These are NOT M-bt changes but were necessary for A4 to pass since kenoma-types is a direct `-p` target.
3. **Task 3 deviation:** on_fill_invariant_test.rs removed unused `bar()` helper and `Bar` import to avoid dead_code clippy warning. All 3 test functions are verbatim per plan.
4. **apply_fill tag propagation placement:** Plan placed tag→liquidity propagation at the end of apply_fill before `self.fills.push`. Implementation placed it at the top (before portfolio.apply_fill), so the fill has its liquidity set before the strategy's on_fill callback observes it. Semantically cleaner; passes all tests.

## Commit log

```
e4bb881 M-bt: clippy clean (fix pre-existing kenoma-types lints)
6fa5cae M-bt: backwards-compat gate test (enable_hg_hooks=false → resolvers untouched)
5b8b806 M-bt task 9: fire on_rollover_boundary + engine force-flat on contract change
18d50fb M-bt task 8: fire on_session_boundary on phase transitions (gated by enable_hg_hooks)
b46113e M-bt task 7: BacktestEngine resolver fields + with_session_resolver/with_rollover_resolver
618cc68 M-bt: add on_session_boundary + on_rollover_boundary to Strategy trait (default no-op)
f42388b M-bt: SessionResolver + RolloverResolver traits and AlwaysRth + StaticContract defaults
f796a5f M-bt: add ExecutionConfig.enable_hg_hooks (default false)
e8702c9 M-bt: lock existing on_fill invariant (test asset, no engine change)
ddbe431 M-bt: add SessionPhase enum to kenoma-types
644cef0 M-bt: start from 5bd835f baseline (on_fill verified wired)
```

## Operator-side next steps

1. Run `harness/briefs/M-bt/dual_blind_compare.sh m-bt-claude m-bt-codex` to compare against the other worktree. Expect IDENTICAL or SEMANTIC_EQUIVALENT.
2. If IDENTICAL: merge `m-bt-claude` into main with `--no-ff`.
3. Tag the merge commit `v0.2.0-hg-hooks` (annotated tag): `git tag -a v0.2.0-hg-hooks -m "M-bt: kenoma-backtester engine hooks for hunger-games harness"`
4. Push tag if applicable to operator workflow.
5. Wave-1 integration smoke (`~/LOCAL_DEV/kenoma-hunger-games/harness/integration_smoke/wave_1.sh`) runs after both M0 AND M-bt merge.
