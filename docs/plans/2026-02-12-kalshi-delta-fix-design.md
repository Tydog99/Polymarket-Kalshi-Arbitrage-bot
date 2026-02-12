# Kalshi Delta Handler Fix — Shadow Book Design

**Date:** 2026-02-12
**Issue:** #52 (phantom liquidity), #54 (root cause: delta handler reads wrong fields)
**Branch:** `fix/kalshi-orderbook-fix`

## Problem

`process_kalshi_delta()` reads `body.yes`/`body.no` (snapshot array fields), but Kalshi delta messages use `body.price`/`body.delta`/`body.side`. Every delta is silently ignored — the `AtomicOrderbook` cache freezes at the last snapshot value.

This caused the 2026-02-12 incident: $1.41 realized losses, $11.41 open unhedged exposure, 165+ spam HTTP requests, all from executing against stale prices.

## Approach: Shadow Book

Add a `BTreeMap<i64, i64>` (price → quantity) per side per market behind `parking_lot::Mutex`. The delta handler applies changes to the BTreeMap, recomputes best bid, and updates the existing `AtomicOrderbook` which continues serving lock-free reads.

### Why BTreeMap?

A delta says "quantity at price X changed by Y." To know the new best bid after a change, you need all levels. `BTreeMap::iter().next_back()` gives O(1) access to the max key after any insert/remove.

### Why not replace AtomicOrderbook?

`AtomicOrderbook` provides zero-contention reads for arb detection on WebSocket threads. The BTreeMap is a write-side concern only — it feeds the atomic cache. No changes needed to arb detection, execution, or any downstream code.

## Changes

### 1. New struct: `KalshiBook` (types.rs)

```rust
use parking_lot::Mutex;
use std::collections::BTreeMap;

pub struct KalshiBook {
    pub yes_bids: Mutex<BTreeMap<i64, i64>>,  // price → qty
    pub no_bids: Mutex<BTreeMap<i64, i64>>,
}
```

Added as a field on `AtomicMarketState`.

### 2. Fix `process_kalshi_delta` (kalshi.rs)

**Before:** Reads `body.yes`/`body.no` (always None on deltas) → preserves stale values.

**After:**
1. Read `body.price`, `body.delta`, `body.side`
2. Apply delta to the appropriate BTreeMap: `bids[price] += delta`, remove if ≤ 0
3. Recompute best bid: `bids.iter().next_back()`
4. Derive ask: `ask = 100 - best_bid_price`
5. Compute size: `size = qty * price / 100`
6. Update `AtomicOrderbook` via `update_yes()` or `update_no()`
7. If book is empty after delta: set ask and size to 0

### 3. Update `process_kalshi_snapshot` (kalshi.rs)

**Before:** Only updates `AtomicOrderbook`.

**After:** Also clears and rebuilds both BTreeMaps from the snapshot data, then updates `AtomicOrderbook` from the rebuilt book.

### 4. No changes to:
- `AtomicOrderbook` struct or API
- `ArbOpportunity::detect()`
- `ExecutionEngine`
- Any downstream code

## Testing

### Unit Tests (kalshi.rs or types.rs)

| Test | What it verifies |
|------|-----------------|
| `snapshot_populates_book` | Snapshot data correctly populates BTreeMap and AtomicOrderbook |
| `delta_adds_liquidity` | Positive delta at non-best level adds entry without changing best |
| `delta_removes_best_bid` | Removing best bid falls back to next-best, AtomicOrderbook updates |
| `delta_empties_book` | All bids removed → AtomicOrderbook reads (0, 0) for that side |
| `delta_at_unknown_price` | Positive delta creates new level; negative delta at absent price is no-op |
| `snapshot_resets_after_deltas` | Snapshot clears delta-accumulated state |

### Property Tests (randomized, using `rand`)

```rust
#[test]
fn fuzz_delta_invariants() {
    for _ in 0..1000 {
        // Generate random snapshot (3-10 levels)
        // Apply 5-20 random deltas
        // After each op, verify:
        //   1. AtomicOrderbook matches BTreeMap best
        //   2. No BTreeMap entry has qty ≤ 0
        //   3. Empty book → zero prices
    }
}
```

### Regression Test (incident #52)

Simulate the exact incident sequence:
1. Snapshot: YES bids at 36¢ (qty 23) → `no_ask = 64¢`
2. Delta: `{price: 36, delta: -23, side: "yes"}` (cancel best bid)
3. Verify: `no_ask` either falls to next-best or goes to 0
4. Verify: `ArbOpportunity::detect()` returns `None` (no phantom arb)

### Existing Proof Tests (issue #54)

The 3 `kalshi_delta_bug_proof` tests in `integration_tests.rs` currently prove the bug exists. After the fix, convert them to prove it's resolved (or remove if redundant with new unit tests).

## Risk Assessment

- **Mutex contention:** Zero in practice. Only the Kalshi WebSocket handler writes, on a single tokio task.
- **Memory:** ~200 bytes per market for typical 5-20 levels. Negligible at 1000 markets.
- **Correctness:** BTreeMap is well-tested stdlib. Snapshot resets prevent drift. Property tests verify invariants.
- **Performance:** `parking_lot::Mutex` uncontended: ~10ns. BTreeMap insert/remove: O(log n) ≈ 4 comparisons. Total delta processing: ~50ns added vs. current (broken) ~0ns.

## Implementation Order

1. Add `KalshiBook` struct to `types.rs`, add field to `AtomicMarketState`
2. Update `process_kalshi_snapshot` to populate BTreeMaps
3. Rewrite `process_kalshi_delta` to read correct fields and apply to BTreeMaps
4. Write unit tests
5. Write property/fuzz test
6. Write regression test for #52 scenario
7. Verify existing tests pass
