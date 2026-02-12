# Fix Kalshi Delta Handler Bug (Issue #54)

## Context

`process_kalshi_delta()` in `kalshi.rs:783` reads `body.yes`/`body.no` (snapshot fields), but Kalshi delta messages use `body.price`/`body.delta`/`body.side`. Every delta is silently ignored — the orderbook cache only updates on full snapshots. This caused the 2026-02-12 phantom liquidity incident ($1.41 realized loss, $11.41 unhedged exposure, 165+ spam orders).

The fix adds per-market book state (`BTreeMap`) so deltas can be applied incrementally and the best bid recomputed correctly.

## Files to Modify

1. `controller/src/types.rs` — Add `KalshiBook` struct, add field to `AtomicMarketState`
2. `controller/src/kalshi.rs` — Fix `process_kalshi_delta()`, update `process_kalshi_snapshot()`
3. `controller/tests/integration_tests.rs` — Replace bug-proof tests with correctness tests

## Step 1: Add `KalshiBook` to `types.rs`

**1a.** Add `use std::collections::BTreeMap;` to imports (already has `HashMap`). Add `use parking_lot::Mutex;` (already has `RwLock` from parking_lot).

**1b.** Define `KalshiBook` struct (after `AtomicOrderbook` impl ~line 162, before `AtomicMarketState`):

```rust
pub struct KalshiBook {
    pub yes_bids: BTreeMap<i64, i64>,  // price -> qty
    pub no_bids: BTreeMap<i64, i64>,
}
```

Methods:
- `new()` — empty book
- `set_yes_bids(&mut self, levels: &[Vec<i64>])` — clear + populate from snapshot array
- `set_no_bids(&mut self, levels: &[Vec<i64>])` — same for NO
- `apply_yes_delta(&mut self, price: i64, delta: i64)` — add delta to entry, remove if <= 0
- `apply_no_delta(&mut self, price: i64, delta: i64)` — same for NO
- `best_yes_bid(&self) -> Option<(i64, i64)>` — `iter().next_back()` (highest key in BTreeMap)
- `best_no_bid(&self) -> Option<(i64, i64)>` — same for NO
- `derive_no_side(&self) -> (PriceCents, SizeCents)` — from best YES bid: `ask = 100 - price`, `size = qty * price / 100`
- `derive_yes_side(&self) -> (PriceCents, SizeCents)` — from best NO bid

**1c.** Add field to `AtomicMarketState` (line 165):

```rust
pub kalshi_book: Mutex<KalshiBook>,
```

**1d.** Initialize in `AtomicMarketState::new()` (line 186):

```rust
kalshi_book: Mutex::new(KalshiBook::new()),
```

**Why `Mutex` not `RwLock`:** Only the single Kalshi WS thread accesses the BTreeMap. No readers need it — arb detection reads `AtomicOrderbook` instead.

**Why `BTreeMap`:** `iter().next_back()` gives O(log n) access to highest key (best bid). Books have ~5-20 levels, so memory is ~1024 markets * 40 levels * 16 bytes = ~640KB worst case.

## Step 2: Update `process_kalshi_snapshot()` in `kalshi.rs` (lines 706-778)

Make the snapshot handler populate the `KalshiBook` first, then derive the `AtomicOrderbook` from it. This ensures snapshot and delta paths use the same source of truth.

Key changes:
- Lock `market.kalshi_book`
- Call `book.set_yes_bids(levels)` / `book.set_no_bids(levels)` (clears old state first)
- Derive `(no_ask, no_size)` via `book.derive_no_side()`, `(yes_ask, yes_size)` via `book.derive_yes_side()`
- Drop the lock, then `market.kalshi.store(yes_ask, no_ask, yes_size, no_size)`

## Step 3: Rewrite `process_kalshi_delta()` in `kalshi.rs` (lines 783-849)

Complete rewrite to read the correct delta fields:

1. Extract `body.price`, `body.delta`, `body.side` — return early with warning if missing
2. Lock `market.kalshi_book`
3. Match on `side`:
   - `"yes"` → `book.apply_yes_delta(price, delta)`, then `book.derive_no_side()` → `market.kalshi.update_no(no_ask, no_size)`
   - `"no"` → `book.apply_no_delta(price, delta)`, then `book.derive_yes_side()` → `market.kalshi.update_yes(yes_ask, yes_size)`
4. Drop lock before atomic store
5. Use `update_yes()`/`update_no()` (CAS) instead of `store()` — only updates the affected side, preserving the other

## Step 4: Replace proof tests in `integration_tests.rs` (lines 2443-2574)

Replace `kalshi_delta_bug_proof` module with `kalshi_delta_correctness` module. Tests use `KalshiBook` directly (since `process_kalshi_delta` is private):

| Test | Proves |
|------|--------|
| `test_cancel_at_best_bid_clears_price` | Cancelling only bid → price goes to 0 |
| `test_cancel_best_reveals_next_best` | Cancelling best → next-best becomes active |
| `test_no_phantom_arb_after_cancel` | End-to-end: arb disappears after cancel |
| `test_delta_adds_new_level` | Adding liquidity via delta works |
| `test_delta_partial_reduction` | Reducing quantity updates size correctly |
| `test_non_best_level_delta_preserves_best` | Delta on non-best level doesn't corrupt best |
| `test_snapshot_resets_book` | Snapshot after deltas fully resets state |

## Step 5: Add unit tests for `KalshiBook` in `types.rs`

Add to existing `#[cfg(test)] mod tests` block:
- `test_kalshi_book_set_and_best_bid`
- `test_kalshi_book_empty`
- `test_kalshi_book_apply_delta_add` / `_remove` / `_negative_cleans_up`
- `test_kalshi_book_derive_no_side` / `_yes_side`
- `test_kalshi_book_clear`
- `test_kalshi_book_skips_zero_qty_on_set`

## Verification

```bash
# 1. Build
cargo build --release

# 2. Run all tests
cargo test

# 3. Specifically run the new delta correctness tests
cargo test kalshi_delta_correctness

# 4. Run KalshiBook unit tests
cargo test kalshi_book

# 5. Check for compiler warnings
cargo build --release 2>&1 | grep warning
```
