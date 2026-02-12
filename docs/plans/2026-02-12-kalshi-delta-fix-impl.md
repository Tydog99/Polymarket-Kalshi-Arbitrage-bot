# Kalshi Delta Handler Fix — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix `process_kalshi_delta()` so it reads the correct delta fields (`body.price`/`body.delta`/`body.side`) and applies them to a per-market orderbook, instead of reading always-None snapshot fields.

**Architecture:** Add a `KalshiBook` (BTreeMap per side behind `parking_lot::Mutex`) as the write-side source of truth. Snapshots and deltas update the BTreeMap, then recompute best bid and push to the existing `AtomicOrderbook` (lock-free u64) which continues serving reads for arb detection. No downstream code changes.

**Tech Stack:** Rust, `parking_lot::Mutex`, `std::collections::BTreeMap`, `rand` (for property tests). All deps already in `Cargo.toml`.

---

## Background: How Kalshi Orderbooks Work

Kalshi sends **bids** (not asks). The bot derives asks:
- Best YES bid at price P → NO ask = `100 - P`, size = `qty * P / 100`
- Best NO bid at price P → YES ask = `100 - P`, size = `qty * P / 100`

Two message types:
- **`orderbook_snapshot`**: `body.yes` / `body.no` = arrays of `[price, qty]`
- **`orderbook_delta`**: `body.price` / `body.delta` / `body.side` = single-level change

The bug: `process_kalshi_delta()` reads `body.yes`/`body.no` (always `None` on deltas), so every delta is silently ignored.

---

### Task 1: Add `KalshiBook` struct to `types.rs`

**Files:**
- Modify: `controller/src/types.rs:11` (imports), `controller/src/types.rs:162-196` (new struct + AtomicMarketState changes)

**Step 1: Write the failing test**

Add to the bottom of the `#[cfg(test)] mod tests` block in `controller/src/types.rs` (before the closing `}` at line 1856):

```rust
    // =========================================================================
    // KalshiBook Tests
    // =========================================================================

    #[test]
    fn test_kalshi_book_new_is_empty() {
        let book = KalshiBook::new();
        assert!(book.yes_bids.lock().is_empty());
        assert!(book.no_bids.lock().is_empty());
        assert_eq!(book.best_yes_bid(), None);
        assert_eq!(book.best_no_bid(), None);
    }

    #[test]
    fn test_kalshi_book_set_yes_bids() {
        let mut book = KalshiBook::new();
        let levels = vec![vec![36, 23], vec![34, 10], vec![30, 5]];
        book.set_yes_bids(&levels);

        let bids = book.yes_bids.lock();
        assert_eq!(bids.len(), 3);
        assert_eq!(bids[&36], 23);
        assert_eq!(bids[&34], 10);
        assert_eq!(bids[&30], 5);
        drop(bids);

        // Best bid is highest price
        assert_eq!(book.best_yes_bid(), Some((36, 23)));
    }

    #[test]
    fn test_kalshi_book_set_bids_skips_zero_qty() {
        let mut book = KalshiBook::new();
        let levels = vec![vec![36, 23], vec![34, 0], vec![30, 5]];
        book.set_yes_bids(&levels);

        let bids = book.yes_bids.lock();
        assert_eq!(bids.len(), 2);
        assert!(!bids.contains_key(&34));
    }

    #[test]
    fn test_kalshi_book_set_bids_clears_previous() {
        let mut book = KalshiBook::new();
        book.set_yes_bids(&[vec![50, 10]]);
        assert_eq!(book.best_yes_bid(), Some((50, 10)));

        // Set new bids — old ones should be gone
        book.set_yes_bids(&[vec![40, 5]]);
        assert_eq!(book.best_yes_bid(), Some((40, 5)));
        assert_eq!(book.yes_bids.lock().len(), 1);
    }

    #[test]
    fn test_kalshi_book_apply_delta_add() {
        let mut book = KalshiBook::new();
        book.set_yes_bids(&[vec![36, 23]]);

        // Add 10 contracts at price 34
        book.apply_delta("yes", 34, 10);

        let bids = book.yes_bids.lock();
        assert_eq!(bids[&34], 10);
        assert_eq!(bids[&36], 23); // unchanged
    }

    #[test]
    fn test_kalshi_book_apply_delta_increase() {
        let mut book = KalshiBook::new();
        book.set_yes_bids(&[vec![36, 23]]);

        // Add 5 more contracts at existing level
        book.apply_delta("yes", 36, 5);
        assert_eq!(book.yes_bids.lock()[&36], 28);
    }

    #[test]
    fn test_kalshi_book_apply_delta_remove() {
        let mut book = KalshiBook::new();
        book.set_yes_bids(&[vec![36, 23], vec![34, 10]]);

        // Remove all 23 contracts at price 36
        book.apply_delta("yes", 36, -23);

        let bids = book.yes_bids.lock();
        assert!(!bids.contains_key(&36), "Level should be removed when qty hits 0");
        assert_eq!(bids.len(), 1);
        drop(bids);

        // Best bid should now be 34
        assert_eq!(book.best_yes_bid(), Some((34, 10)));
    }

    #[test]
    fn test_kalshi_book_apply_delta_partial_reduce() {
        let mut book = KalshiBook::new();
        book.set_yes_bids(&[vec![36, 23]]);

        book.apply_delta("yes", 36, -10);
        assert_eq!(book.yes_bids.lock()[&36], 13);
    }

    #[test]
    fn test_kalshi_book_apply_delta_overshoot_removes() {
        let mut book = KalshiBook::new();
        book.set_yes_bids(&[vec![36, 5]]);

        // Remove more than exists — should clean up the level
        book.apply_delta("yes", 36, -10);
        assert!(book.yes_bids.lock().is_empty());
    }

    #[test]
    fn test_kalshi_book_apply_delta_at_absent_price() {
        let mut book = KalshiBook::new();
        book.set_yes_bids(&[vec![36, 23]]);

        // Negative delta at price not in book — no-op
        book.apply_delta("yes", 20, -5);
        assert_eq!(book.yes_bids.lock().len(), 1); // unchanged
    }

    #[test]
    fn test_kalshi_book_apply_delta_no_side() {
        let mut book = KalshiBook::new();
        book.set_no_bids(&[vec![60, 15]]);

        book.apply_delta("no", 58, 8);
        assert_eq!(book.no_bids.lock()[&58], 8);
        assert_eq!(book.best_no_bid(), Some((60, 15)));
    }

    #[test]
    fn test_kalshi_book_derive_no_side() {
        let mut book = KalshiBook::new();
        // YES bid at 36¢ with 23 contracts → NO ask = 64¢, size = 23*36/100 = 8
        book.set_yes_bids(&[vec![36, 23], vec![34, 10]]);

        let (no_ask, no_size) = book.derive_no_side();
        assert_eq!(no_ask, 64); // 100 - 36
        assert_eq!(no_size, 8); // 23 * 36 / 100 = 8 (integer division)
    }

    #[test]
    fn test_kalshi_book_derive_yes_side() {
        let mut book = KalshiBook::new();
        // NO bid at 60¢ with 15 contracts → YES ask = 40¢, size = 15*60/100 = 9
        book.set_no_bids(&[vec![60, 15], vec![58, 8]]);

        let (yes_ask, yes_size) = book.derive_yes_side();
        assert_eq!(yes_ask, 40); // 100 - 60
        assert_eq!(yes_size, 9); // 15 * 60 / 100 = 9
    }

    #[test]
    fn test_kalshi_book_derive_empty_book() {
        let book = KalshiBook::new();
        assert_eq!(book.derive_no_side(), (0, 0));
        assert_eq!(book.derive_yes_side(), (0, 0));
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p controller --lib kalshi_book -- --nocapture 2>&1 | head -30`
Expected: FAIL — `KalshiBook` type doesn't exist yet.

**Step 3: Write the `KalshiBook` implementation**

In `controller/src/types.rs`:

3a. Add `Mutex` import at line 11 (alongside existing `parking_lot::RwLock`):

```rust
use parking_lot::{Mutex, RwLock};
```

3b. Add `BTreeMap` to the existing `std::collections` import. Find `use std::collections::HashMap;` and change to:

```rust
use std::collections::{BTreeMap, HashMap};
```

3c. Add `KalshiBook` struct and impl after `AtomicOrderbook` (after line 162, before `AtomicMarketState`):

```rust
/// Full Kalshi orderbook for correct delta processing.
///
/// Stores all bid levels per side as BTreeMap (price → quantity).
/// Only the Kalshi WebSocket thread writes; arb detection reads
/// from `AtomicOrderbook` instead (lock-free). This struct is the
/// source of truth that feeds the atomic cache after each update.
pub struct KalshiBook {
    pub yes_bids: Mutex<BTreeMap<i64, i64>>,
    pub no_bids: Mutex<BTreeMap<i64, i64>>,
}

impl KalshiBook {
    pub fn new() -> Self {
        Self {
            yes_bids: Mutex::new(BTreeMap::new()),
            no_bids: Mutex::new(BTreeMap::new()),
        }
    }

    /// Clear and populate YES bids from a snapshot array of [price, qty] pairs.
    /// Entries with qty <= 0 are skipped.
    pub fn set_yes_bids(&mut self, levels: &[Vec<i64>]) {
        let mut bids = self.yes_bids.lock();
        bids.clear();
        for l in levels {
            if l.len() >= 2 && l[1] > 0 {
                bids.insert(l[0], l[1]);
            }
        }
    }

    /// Clear and populate NO bids from a snapshot array of [price, qty] pairs.
    pub fn set_no_bids(&mut self, levels: &[Vec<i64>]) {
        let mut bids = self.no_bids.lock();
        bids.clear();
        for l in levels {
            if l.len() >= 2 && l[1] > 0 {
                bids.insert(l[0], l[1]);
            }
        }
    }

    /// Apply a delta to the book. `side` is "yes" or "no".
    /// Positive delta adds contracts, negative removes.
    /// Levels with qty <= 0 after the delta are removed.
    pub fn apply_delta(&mut self, side: &str, price: i64, delta: i64) {
        let mut bids = match side {
            "yes" => self.yes_bids.lock(),
            "no" => self.no_bids.lock(),
            _ => return,
        };
        let entry = bids.entry(price).or_insert(0);
        *entry += delta;
        if *entry <= 0 {
            bids.remove(&price);
        }
    }

    /// Returns the best (highest-price) YES bid as (price, qty), or None.
    pub fn best_yes_bid(&self) -> Option<(i64, i64)> {
        self.yes_bids.lock().iter().next_back().map(|(&p, &q)| (p, q))
    }

    /// Returns the best (highest-price) NO bid as (price, qty), or None.
    pub fn best_no_bid(&self) -> Option<(i64, i64)> {
        self.no_bids.lock().iter().next_back().map(|(&p, &q)| (p, q))
    }

    /// Derive NO ask price and size from the best YES bid.
    /// Returns (0, 0) if the YES side is empty.
    pub fn derive_no_side(&self) -> (PriceCents, SizeCents) {
        match self.best_yes_bid() {
            Some((price, qty)) => {
                let ask = (100 - price) as PriceCents;
                let size = (qty * price / 100) as SizeCents;
                (ask, size)
            }
            None => (0, 0),
        }
    }

    /// Derive YES ask price and size from the best NO bid.
    /// Returns (0, 0) if the NO side is empty.
    pub fn derive_yes_side(&self) -> (PriceCents, SizeCents) {
        match self.best_no_bid() {
            Some((price, qty)) => {
                let ask = (100 - price) as PriceCents;
                let size = (qty * price / 100) as SizeCents;
                (ask, size)
            }
            None => (0, 0),
        }
    }
}
```

3d. Add `kalshi_book` field to `AtomicMarketState` struct (line 165, add after `pub poly: AtomicOrderbook,`):

```rust
    /// Shadow orderbook for Kalshi delta processing (source of truth for bids)
    pub kalshi_book: Mutex<KalshiBook>,
```

3e. Initialize in `AtomicMarketState::new()` (line 186, in the `Self { ... }` block):

```rust
            kalshi_book: Mutex::new(KalshiBook::new()),
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p controller --lib kalshi_book -- --nocapture`
Expected: All 14 `kalshi_book` tests PASS.

**Step 5: Commit**

```bash
git add controller/src/types.rs
git commit -m "feat(types): add KalshiBook shadow orderbook for delta processing

Adds BTreeMap-based orderbook per side behind parking_lot::Mutex.
This will be the source of truth for Kalshi delta application,
feeding the existing AtomicOrderbook lock-free cache.

Fixes #54"
```

---

### Task 2: Update `process_kalshi_snapshot()` to populate KalshiBook

**Files:**
- Modify: `controller/src/kalshi.rs:706-778` (snapshot handler)

**Context:** `process_kalshi_snapshot` currently reads `body.yes`/`body.no` arrays and computes best bids inline. We need it to also populate the `KalshiBook` so that subsequent deltas have the correct starting state.

**Step 1: Write the failing test**

Add to `controller/src/types.rs` test module (bottom of tests, before closing `}`):

```rust
    #[test]
    fn test_snapshot_populates_kalshi_book_and_atomic() {
        let market = AtomicMarketState::new(0);

        // Simulate what process_kalshi_snapshot should do:
        // populate KalshiBook then derive AtomicOrderbook
        {
            let mut book = market.kalshi_book.lock();
            book.set_yes_bids(&[vec![36, 23], vec![34, 10], vec![30, 5]]);
            book.set_no_bids(&[vec![60, 15], vec![58, 8]]);

            let (no_ask, no_size) = book.derive_no_side();
            let (yes_ask, yes_size) = book.derive_yes_side();
            drop(book);

            market.kalshi.store(yes_ask, no_ask, yes_size, no_size);
        }

        // Verify AtomicOrderbook
        let (yes_ask, no_ask, yes_size, no_size) = market.kalshi.load();
        assert_eq!(yes_ask, 40);  // 100 - 60 (best NO bid)
        assert_eq!(no_ask, 64);   // 100 - 36 (best YES bid)
        assert_eq!(yes_size, 9);  // 15 * 60 / 100
        assert_eq!(no_size, 8);   // 23 * 36 / 100

        // Verify KalshiBook state
        let book = market.kalshi_book.lock();
        assert_eq!(book.yes_bids.lock().len(), 3);
        assert_eq!(book.no_bids.lock().len(), 2);
    }
```

**Step 2: Run test to verify it passes** (this test exercises KalshiBook API, not the handler directly)

Run: `cargo test -p controller --lib snapshot_populates -- --nocapture`
Expected: PASS (it uses the KalshiBook API we already implemented).

**Step 3: Rewrite `process_kalshi_snapshot` in `kalshi.rs`**

Replace the entire function body at lines 706-778 with:

```rust
fn process_kalshi_snapshot(market: &crate::types::AtomicMarketState, body: &KalshiWsMsgBody) {
    let ticker = body.market_ticker.as_deref().unwrap_or("unknown");

    // Populate KalshiBook from snapshot arrays
    let mut book = market.kalshi_book.lock();

    if let Some(levels) = &body.yes {
        tracing::debug!(
            "[KALSHI-SNAP] {} | YES bids: {:?}",
            ticker, levels
        );
        book.set_yes_bids(levels);
    } else {
        book.set_yes_bids(&[]);
    }

    if let Some(levels) = &body.no {
        tracing::debug!(
            "[KALSHI-SNAP] {} | NO bids: {:?}",
            ticker, levels
        );
        book.set_no_bids(levels);
    } else {
        book.set_no_bids(&[]);
    }

    // Derive AtomicOrderbook from book state
    let (no_ask, no_size) = book.derive_no_side();
    let (yes_ask, yes_size) = book.derive_yes_side();
    drop(book);

    tracing::debug!(
        "[KALSHI-SNAP] {} | COMPUTED: yes_ask={}¢ no_ask={}¢ yes_size={}¢ no_size={}¢",
        ticker, yes_ask, no_ask, yes_size, no_size
    );

    market.kalshi.store(yes_ask, no_ask, yes_size, no_size);
    market.inc_kalshi_updates();
}
```

**Step 4: Run full test suite**

Run: `cargo test -p controller`
Expected: All existing tests PASS. The snapshot handler produces the same output as before (same math, different code path).

**Step 5: Commit**

```bash
git add controller/src/kalshi.rs
git commit -m "refactor(kalshi): snapshot handler populates KalshiBook then derives AtomicOrderbook

Snapshot now writes to KalshiBook (BTreeMap) first, then derives
the AtomicOrderbook from the book state. This ensures deltas
will have the correct starting state after a snapshot.

Same mathematical output — no behavior change for snapshots."
```

---

### Task 3: Rewrite `process_kalshi_delta()` to read correct fields

**Files:**
- Modify: `controller/src/kalshi.rs:783-849` (delta handler)

**Step 1: Write the failing integration test**

**Note:** Commit `ad7abcf8` on `fix/confirm-stale-prices` has 3 proof tests in a `kalshi_delta_bug_proof` module that prove the bug exists by asserting the *broken* behavior. Those tests are NOT on our branch. Our new `kalshi_delta_correctness` module covers the same scenarios but asserts the *correct* (fixed) behavior. We do NOT cherry-pick the old proof tests — they would fail after the fix (by design).

Add a new test module at the bottom of `controller/tests/integration_tests.rs`:

```rust
// =============================================================================
// Kalshi Delta Correctness Tests (Issue #54 fix verification)
// =============================================================================

mod kalshi_delta_correctness {
    use arb_bot::types::{AtomicMarketState, ArbOpportunity, KalshiBook};
    use arb_bot::arb::ArbConfig;

    /// Helper: simulate a snapshot by populating KalshiBook + AtomicOrderbook
    fn apply_snapshot(market: &AtomicMarketState, yes_bids: &[Vec<i64>], no_bids: &[Vec<i64>]) {
        let mut book = market.kalshi_book.lock();
        book.set_yes_bids(yes_bids);
        book.set_no_bids(no_bids);
        let (no_ask, no_size) = book.derive_no_side();
        let (yes_ask, yes_size) = book.derive_yes_side();
        drop(book);
        market.kalshi.store(yes_ask, no_ask, yes_size, no_size);
    }

    /// Helper: simulate a delta by applying to KalshiBook + updating AtomicOrderbook
    fn apply_delta(market: &AtomicMarketState, side: &str, price: i64, delta: i64) {
        let mut book = market.kalshi_book.lock();
        book.apply_delta(side, price, delta);
        match side {
            "yes" => {
                let (no_ask, no_size) = book.derive_no_side();
                drop(book);
                market.kalshi.update_no(no_ask, no_size);
            }
            "no" => {
                let (yes_ask, yes_size) = book.derive_yes_side();
                drop(book);
                market.kalshi.update_yes(yes_ask, yes_size);
            }
            _ => {}
        }
    }

    #[test]
    fn test_cancel_at_best_bid_clears_price() {
        let market = AtomicMarketState::new(0);
        // Only one YES bid at 36 with qty 23
        apply_snapshot(&market, &[vec![36, 23]], &[vec![60, 15]]);

        let (_, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 64); // 100 - 36

        // Cancel the only bid
        apply_delta(&market, "yes", 36, -23);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 0, "NO ask should be 0 when all YES bids are gone");
        assert_eq!(no_size, 0);
    }

    #[test]
    fn test_cancel_best_reveals_next_best() {
        let market = AtomicMarketState::new(0);
        // Two YES bid levels
        apply_snapshot(&market, &[vec![36, 23], vec![34, 10]], &[vec![60, 15]]);

        let (_, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 64); // 100 - 36

        // Cancel the best bid (36)
        apply_delta(&market, "yes", 36, -23);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 66, "NO ask should fall back to 100 - 34 = 66");
        assert_eq!(no_size, 3); // 10 * 34 / 100 = 3
    }

    #[test]
    fn test_no_phantom_arb_after_cancel() {
        let market = AtomicMarketState::new(0);
        let config = ArbConfig::default();

        // Set up: Kalshi YES bids create a YES ask of 40¢ (NO bid at 60)
        // and NO ask of 64¢ (YES bid at 36)
        apply_snapshot(&market, &[vec![36, 23]], &[vec![60, 15]]);

        // Set Poly prices that would create an arb with Kalshi NO at 64¢
        // Poly YES at 34¢ + Kalshi NO at 64¢ = 98¢ + ~1¢ fee = 99¢ <= 99¢ threshold
        market.poly.store(34, 70, 500, 500);

        // Verify arb exists before cancel
        let arb = ArbOpportunity::detect(0, market.kalshi.load(), market.poly.load(), &config, 0);
        assert!(arb.is_some(), "Arb should exist before cancel");

        // Cancel the YES bid at 36 — kills the Kalshi NO side
        apply_delta(&market, "yes", 36, -23);

        // Now Kalshi NO ask is 0 (no bids) — arb should disappear
        let arb = ArbOpportunity::detect(0, market.kalshi.load(), market.poly.load(), &config, 0);
        assert!(arb.is_none(), "Phantom arb should NOT exist after bid cancellation");
    }

    #[test]
    fn test_delta_adds_new_level() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23]], &[vec![60, 15]]);

        // Add a higher bid — should become new best
        apply_delta(&market, "yes", 40, 10);

        let (_, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 60, "NO ask should update to 100 - 40 = 60");
    }

    #[test]
    fn test_delta_partial_reduction() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23]], &[vec![60, 15]]);

        // Reduce quantity at best bid but don't remove it
        apply_delta(&market, "yes", 36, -10);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 64); // price unchanged
        assert_eq!(no_size, 4); // 13 * 36 / 100 = 4
    }

    #[test]
    fn test_non_best_level_delta_preserves_best() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23], vec![34, 10]], &[vec![60, 15]]);

        // Delta on non-best level (34) doesn't change NO ask
        apply_delta(&market, "yes", 34, -5);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 64, "NO ask should still reflect best bid at 36");
        assert_eq!(no_size, 8); // 23 * 36 / 100 = 8 (unchanged)
    }

    #[test]
    fn test_snapshot_resets_book() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23], vec![34, 10]], &[vec![60, 15]]);

        // Apply several deltas
        apply_delta(&market, "yes", 36, -23); // remove best
        apply_delta(&market, "yes", 38, 50);  // add new level

        // Now send a new snapshot — should completely reset
        apply_snapshot(&market, &[vec![42, 30]], &[vec![55, 20]]);

        let (yes_ask, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 58); // 100 - 42
        assert_eq!(yes_ask, 45); // 100 - 55

        // Old delta state should be gone
        let book = market.kalshi_book.lock();
        assert_eq!(book.yes_bids.lock().len(), 1); // only [42, 30]
        assert!(!book.yes_bids.lock().contains_key(&38)); // delta-added level gone
    }
}
```

**Step 2: Run test to verify it compiles and the helpers work**

Run: `cargo test -p controller --test integration_tests kalshi_delta_correctness -- --nocapture`
Expected: All PASS — these tests use the `KalshiBook` API directly (simulating what the fixed handler will do), not the actual `process_kalshi_delta` function.

**Step 3: Rewrite `process_kalshi_delta` in `kalshi.rs`**

Replace the entire function body at lines 783-849 with:

```rust
fn process_kalshi_delta(market: &crate::types::AtomicMarketState, body: &KalshiWsMsgBody) {
    let ticker = body.market_ticker.as_deref().unwrap_or("unknown");

    // Delta messages use price/delta/side fields (NOT yes/no arrays)
    let (price, delta, side) = match (&body.price, &body.delta, &body.side) {
        (Some(p), Some(d), Some(s)) => (*p, *d, s.as_str()),
        _ => {
            tracing::warn!(
                "[KALSHI-DELTA] {} | Malformed delta: missing price/delta/side fields",
                ticker
            );
            return;
        }
    };

    tracing::debug!(
        "[KALSHI-DELTA] {} | side={} price={} delta={}",
        ticker, side, price, delta
    );

    let mut book = market.kalshi_book.lock();
    book.apply_delta(side, price, delta);

    match side {
        "yes" => {
            // YES bid changed → recompute NO ask
            let (no_ask, no_size) = book.derive_no_side();
            drop(book);
            market.kalshi.update_no(no_ask, no_size);
        }
        "no" => {
            // NO bid changed → recompute YES ask
            let (yes_ask, yes_size) = book.derive_yes_side();
            drop(book);
            market.kalshi.update_yes(yes_ask, yes_size);
        }
        _ => {
            tracing::warn!(
                "[KALSHI-DELTA] {} | Unknown side: {}",
                ticker, side
            );
        }
    }

    market.inc_kalshi_updates();
}
```

**Step 4: Run full test suite**

Run: `cargo test -p controller`
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add controller/src/kalshi.rs
git commit -m "fix(kalshi): delta handler reads correct fields and applies to KalshiBook

BREAKING BUG FIX: process_kalshi_delta() was reading body.yes/body.no
(snapshot fields, always None on deltas) instead of body.price/body.delta/
body.side. Every delta was silently ignored, freezing the orderbook cache
at the last snapshot value.

Now reads the correct fields, applies deltas to KalshiBook (BTreeMap),
recomputes best bid, and updates AtomicOrderbook via CAS.

Fixes #54, root cause of #52"
```

---

### Task 4: Add property/fuzz test for delta invariants

**Files:**
- Modify: `controller/src/types.rs` (add to test module)

**Step 1: Write the property test**

Add to the `#[cfg(test)] mod tests` block in `controller/src/types.rs`:

```rust
    // =========================================================================
    // KalshiBook Property/Fuzz Tests
    // =========================================================================

    #[test]
    fn fuzz_kalshi_book_delta_invariants() {
        use rand::Rng;

        let mut rng = rand::thread_rng();

        for iteration in 0..1000 {
            let market = AtomicMarketState::new(0);

            // Generate random snapshot: 3-10 YES levels, 3-10 NO levels
            let num_yes = rng.gen_range(3..=10);
            let num_no = rng.gen_range(3..=10);

            let mut yes_levels: Vec<Vec<i64>> = Vec::new();
            let mut used_prices = std::collections::HashSet::new();
            for _ in 0..num_yes {
                let price: i64 = loop {
                    let p = rng.gen_range(1..=99);
                    if used_prices.insert(p) { break p; }
                };
                let qty: i64 = rng.gen_range(1..=200);
                yes_levels.push(vec![price, qty]);
            }

            let mut no_levels: Vec<Vec<i64>> = Vec::new();
            used_prices.clear();
            for _ in 0..num_no {
                let price: i64 = loop {
                    let p = rng.gen_range(1..=99);
                    if used_prices.insert(p) { break p; }
                };
                let qty: i64 = rng.gen_range(1..=200);
                no_levels.push(vec![price, qty]);
            }

            // Apply snapshot
            {
                let mut book = market.kalshi_book.lock();
                book.set_yes_bids(&yes_levels);
                book.set_no_bids(&no_levels);
                let (no_ask, no_size) = book.derive_no_side();
                let (yes_ask, yes_size) = book.derive_yes_side();
                drop(book);
                market.kalshi.store(yes_ask, no_ask, yes_size, no_size);
            }

            // Verify invariant 1: AtomicOrderbook matches book
            verify_invariants(&market, iteration, "post-snapshot");

            // Apply 5-20 random deltas
            let num_deltas = rng.gen_range(5..=20);
            for d in 0..num_deltas {
                let side = if rng.gen_bool(0.5) { "yes" } else { "no" };
                let price: i64 = rng.gen_range(1..=99);
                let delta: i64 = rng.gen_range(-50..=50);
                if delta == 0 { continue; }

                {
                    let mut book = market.kalshi_book.lock();
                    book.apply_delta(side, price, delta);
                    match side {
                        "yes" => {
                            let (no_ask, no_size) = book.derive_no_side();
                            drop(book);
                            market.kalshi.update_no(no_ask, no_size);
                        }
                        "no" => {
                            let (yes_ask, yes_size) = book.derive_yes_side();
                            drop(book);
                            market.kalshi.update_yes(yes_ask, yes_size);
                        }
                        _ => {}
                    }
                }

                verify_invariants(&market, iteration, &format!("delta-{}", d));
            }
        }
    }

    /// Verify all invariants hold for the current market state
    fn verify_invariants(market: &AtomicMarketState, iteration: usize, phase: &str) {
        let book = market.kalshi_book.lock();
        let (yes_ask, no_ask, yes_size, no_size) = market.kalshi.load();

        // Invariant 1: AtomicOrderbook matches KalshiBook best bid
        let (expected_no_ask, expected_no_size) = book.derive_no_side();
        let (expected_yes_ask, expected_yes_size) = book.derive_yes_side();
        assert_eq!(
            no_ask, expected_no_ask,
            "iter={} phase={}: NO ask mismatch: atomic={} book={}",
            iteration, phase, no_ask, expected_no_ask
        );
        assert_eq!(
            no_size, expected_no_size,
            "iter={} phase={}: NO size mismatch", iteration, phase
        );
        assert_eq!(
            yes_ask, expected_yes_ask,
            "iter={} phase={}: YES ask mismatch: atomic={} book={}",
            iteration, phase, yes_ask, expected_yes_ask
        );
        assert_eq!(
            yes_size, expected_yes_size,
            "iter={} phase={}: YES size mismatch", iteration, phase
        );

        // Invariant 2: No BTreeMap entry has qty <= 0
        for (&price, &qty) in book.yes_bids.lock().iter() {
            assert!(
                qty > 0,
                "iter={} phase={}: YES bid at {}¢ has qty {} <= 0",
                iteration, phase, price, qty
            );
        }
        for (&price, &qty) in book.no_bids.lock().iter() {
            assert!(
                qty > 0,
                "iter={} phase={}: NO bid at {}¢ has qty {} <= 0",
                iteration, phase, price, qty
            );
        }

        // Invariant 3: Empty book → zero prices
        if book.yes_bids.lock().is_empty() {
            assert_eq!(
                no_ask, 0,
                "iter={} phase={}: NO ask should be 0 when YES book empty",
                iteration, phase
            );
        }
        if book.no_bids.lock().is_empty() {
            assert_eq!(
                yes_ask, 0,
                "iter={} phase={}: YES ask should be 0 when NO book empty",
                iteration, phase
            );
        }
    }
```

**Step 2: Run the fuzz test**

Run: `cargo test -p controller --lib fuzz_kalshi_book -- --nocapture`
Expected: PASS — 1000 iterations complete with all invariants holding.

**Step 3: Commit**

```bash
git add controller/src/types.rs
git commit -m "test(types): add property/fuzz test for KalshiBook delta invariants

1000 iterations of random snapshots + 5-20 random deltas each.
Verifies after every operation:
  1. AtomicOrderbook matches KalshiBook best bid
  2. No BTreeMap entry has qty <= 0
  3. Empty book → zero prices in AtomicOrderbook"
```

---

### Task 5: Build and run full test suite

**Files:** None (verification only)

**Step 1: Build release**

Run: `cargo build -p controller --release 2>&1 | tail -5`
Expected: `Finished` with no errors.

**Step 2: Run full test suite**

Run: `cargo test -p controller 2>&1 | tail -20`
Expected: All tests pass (existing + new).

**Step 3: Check for new warnings**

Run: `cargo build -p controller --release 2>&1 | grep "warning\[" | sort -u`
Expected: No new warnings from our changes (pre-existing warnings in other files are OK).

**Step 4: Run just the new tests to confirm naming**

Run: `cargo test -p controller kalshi_book -- --nocapture 2>&1 | tail -20`
Run: `cargo test -p controller --test integration_tests kalshi_delta_correctness -- --nocapture 2>&1 | tail -20`
Expected: Both groups pass.

---

## Summary of Changes

| File | Change |
|------|--------|
| `controller/src/types.rs:11` | Add `Mutex` to `parking_lot` import, `BTreeMap` to `std::collections` |
| `controller/src/types.rs:163-250` | New `KalshiBook` struct with 9 methods |
| `controller/src/types.rs:165` | Add `kalshi_book: Mutex<KalshiBook>` to `AtomicMarketState` |
| `controller/src/types.rs:186` | Initialize `kalshi_book` in `AtomicMarketState::new()` |
| `controller/src/types.rs` (tests) | 14 unit tests + 1 fuzz test (1000 iterations) |
| `controller/src/kalshi.rs:706-778` | Snapshot handler populates KalshiBook first |
| `controller/src/kalshi.rs:783-849` | Delta handler reads correct fields, applies to KalshiBook |
| `controller/tests/integration_tests.rs` | 7 integration tests in `kalshi_delta_correctness` module |

**Not changed:** `AtomicOrderbook`, `ArbOpportunity::detect()`, `ExecutionEngine`, or any other downstream code.
