# Polymarket Shadow Orderbook Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a `PolyBook` shadow orderbook so Polymarket prices are never stale — fixing phantom prices, stale sizes, and missed arb invalidations (issue #57).

**Architecture:** `PolyBook` (a `BTreeMap<u16, u16>` per side) sits behind a `Mutex` on each `AtomicMarketState`. Both `process_book()` (snapshots) and `process_price_change()` (deltas) write to `PolyBook` first, then derive the best ask and push it to the existing lock-free `AtomicOrderbook`. Arb detection and execution are unchanged — they read `AtomicOrderbook` as before.

**Tech Stack:** Rust, `BTreeMap`, `parking_lot::Mutex`, existing `AtomicOrderbook` CAS infrastructure.

**Design doc:** `controller/docs/plans/2026-02-12-polymarket-shadow-book-design.md`

---

### Task 1: Add `PolyBook` struct and unit tests to `types.rs`

**Files:**
- Modify: `controller/src/types.rs:7` (imports)
- Modify: `controller/src/types.rs:11` (parking_lot import)
- Modify: `controller/src/types.rs:162-163` (insert PolyBook after AtomicOrderbook Default impl)
- Modify: `controller/src/types.rs:165-196` (add field to AtomicMarketState + new())
- Modify: `controller/src/types.rs:626+` (add unit tests in existing test module)

**Step 1: Write the failing tests**

Add to the bottom of the existing `#[cfg(test)] mod tests` block in `controller/src/types.rs` (before the closing `}`), around line 958:

```rust
    // =========================================================================
    // PolyBook Tests - Shadow orderbook for Polymarket
    // =========================================================================

    #[test]
    fn test_poly_book_new_is_empty() {
        let book = PolyBook::new();
        assert_eq!(book.best_yes_ask(), None);
        assert_eq!(book.best_no_ask(), None);
    }

    #[test]
    fn test_poly_book_set_and_best_ask() {
        let mut book = PolyBook::new();
        // Snapshot with 3 ask levels: 45¢, 47¢, 50¢
        book.set_yes_asks(&[(47, 2000), (45, 1500), (50, 3000)]);
        // Best ask should be lowest price: 45¢
        assert_eq!(book.best_yes_ask(), Some((45, 1500)));
    }

    #[test]
    fn test_poly_book_update_level_insert() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(50, 1000)]);
        // Insert a new better level at 48¢
        book.update_yes_level(48, 500);
        assert_eq!(book.best_yes_ask(), Some((48, 500)));
    }

    #[test]
    fn test_poly_book_update_level_replace() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000)]);
        // Update size at 45¢ from 1000 to 2000
        book.update_yes_level(45, 2000);
        assert_eq!(book.best_yes_ask(), Some((45, 2000)));
    }

    #[test]
    fn test_poly_book_update_level_remove() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000), (50, 2000)]);
        // Remove 45¢ level (size=0)
        book.update_yes_level(45, 0);
        // Best should now be 50¢
        assert_eq!(book.best_yes_ask(), Some((50, 2000)));
    }

    #[test]
    fn test_poly_book_remove_best_reveals_next() {
        let mut book = PolyBook::new();
        book.set_no_asks(&[(42, 500), (45, 1000), (48, 1500)]);
        assert_eq!(book.best_no_ask(), Some((42, 500)));
        // Remove best ask at 42¢
        book.update_no_level(42, 0);
        // Next best: 45¢
        assert_eq!(book.best_no_ask(), Some((45, 1000)));
    }

    #[test]
    fn test_poly_book_remove_last_level() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(50, 1000)]);
        book.update_yes_level(50, 0);
        assert_eq!(book.best_yes_ask(), None);
    }

    #[test]
    fn test_poly_book_snapshot_replaces_all() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000), (50, 2000)]);
        // New snapshot with completely different levels
        book.set_yes_asks(&[(60, 500), (65, 800)]);
        assert_eq!(book.best_yes_ask(), Some((60, 500)));
        // Old levels should be gone — update_yes_level on 45 with size=0 should be a no-op
        book.update_yes_level(45, 0);
        assert_eq!(book.best_yes_ask(), Some((60, 500)));
    }

    #[test]
    fn test_poly_book_skips_zero_price_and_size() {
        let mut book = PolyBook::new();
        // Zero price and zero size should be skipped
        book.set_yes_asks(&[(0, 1000), (45, 0), (50, 2000)]);
        assert_eq!(book.best_yes_ask(), Some((50, 2000)));
    }

    #[test]
    fn test_poly_book_clear() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000)]);
        book.set_no_asks(&[(55, 2000)]);
        book.clear();
        assert_eq!(book.best_yes_ask(), None);
        assert_eq!(book.best_no_ask(), None);
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test poly_book -- --no-capture 2>&1 | head -30`
Expected: FAIL — `PolyBook` doesn't exist yet.

**Step 3: Add imports**

In `controller/src/types.rs:7`, change:
```rust
use std::collections::HashMap;
```
to:
```rust
use std::collections::{BTreeMap, HashMap};
```

In `controller/src/types.rs:11`, change:
```rust
use parking_lot::RwLock;
```
to:
```rust
use parking_lot::{Mutex, RwLock};
```

**Step 4: Add `PolyBook` struct**

Insert between the `AtomicOrderbook` `Default` impl (line 162) and `AtomicMarketState` (line 164). Insert after line 162:

```rust

/// Shadow orderbook for Polymarket.
///
/// Maintains full ask-side depth for YES and NO tokens so the true best ask
/// is always derivable — even after the current best is removed.
///
/// Polymarket sends asks directly (unlike Kalshi bids). Deltas use absolute
/// replacement (`qty = new_size`), not incremental (`qty += delta`).
/// `size = 0` means remove the level.
pub struct PolyBook {
    yes_asks: BTreeMap<u16, u16>,
    no_asks: BTreeMap<u16, u16>,
}

impl PolyBook {
    pub fn new() -> Self {
        Self {
            yes_asks: BTreeMap::new(),
            no_asks: BTreeMap::new(),
        }
    }

    /// Replace all YES ask levels from a book snapshot.
    /// Clears existing state first — snapshots are full replacements.
    pub fn set_yes_asks(&mut self, levels: &[(u16, u16)]) {
        self.yes_asks.clear();
        for &(price, size) in levels {
            if price > 0 && size > 0 {
                self.yes_asks.insert(price, size);
            }
        }
    }

    /// Replace all NO ask levels from a book snapshot.
    pub fn set_no_asks(&mut self, levels: &[(u16, u16)]) {
        self.no_asks.clear();
        for &(price, size) in levels {
            if price > 0 && size > 0 {
                self.no_asks.insert(price, size);
            }
        }
    }

    /// Apply a YES ask level update (absolute replacement).
    /// size = 0 removes the level.
    pub fn update_yes_level(&mut self, price: u16, size: u16) {
        if size == 0 {
            self.yes_asks.remove(&price);
        } else {
            self.yes_asks.insert(price, size);
        }
    }

    /// Apply a NO ask level update (absolute replacement).
    pub fn update_no_level(&mut self, price: u16, size: u16) {
        if size == 0 {
            self.no_asks.remove(&price);
        } else {
            self.no_asks.insert(price, size);
        }
    }

    /// Best YES ask: lowest price in the book.
    /// BTreeMap is ascending, so `iter().next()` is the lowest key.
    pub fn best_yes_ask(&self) -> Option<(u16, u16)> {
        self.yes_asks.iter().next().map(|(&p, &s)| (p, s))
    }

    /// Best NO ask: lowest price in the book.
    pub fn best_no_ask(&self) -> Option<(u16, u16)> {
        self.no_asks.iter().next().map(|(&p, &s)| (p, s))
    }

    /// Clear all state.
    pub fn clear(&mut self) {
        self.yes_asks.clear();
        self.no_asks.clear();
    }
}
```

**Step 5: Add `poly_book` field to `AtomicMarketState`**

In `AtomicMarketState` struct (after `pub poly_updates: AtomicU32,`, currently line 181), add:
```rust
    /// Shadow orderbook for Polymarket ask-side depth
    pub poly_book: Mutex<PolyBook>,
```

In `AtomicMarketState::new()` (after `poly_updates: AtomicU32::new(0),`, currently line 194), add:
```rust
            poly_book: Mutex::new(PolyBook::new()),
```

**Step 6: Run tests to verify they pass**

Run: `cargo test poly_book -- --no-capture`
Expected: All 10 `poly_book` tests PASS.

**Step 7: Commit**

```bash
git add controller/src/types.rs
git commit -m "feat(types): add PolyBook shadow orderbook struct with unit tests

BTreeMap<u16,u16> per side tracks full ask depth so the true best ask
is always derivable after level removals. Fixes issue #57 step 1."
```

---

### Task 2: Add missing fields to `PriceChangeItem`

**Files:**
- Modify: `controller/src/polymarket.rs:49-54`

**Step 1: Update struct**

Change `PriceChangeItem` at `controller/src/polymarket.rs:49-54` from:
```rust
#[derive(Deserialize, Debug)]
pub struct PriceChangeItem {
    pub asset_id: String,
    pub price: Option<String>,
    pub side: Option<String>,
}
```
to:
```rust
#[derive(Deserialize, Debug)]
pub struct PriceChangeItem {
    pub asset_id: String,
    pub price: Option<String>,
    pub side: Option<String>,
    pub size: Option<String>,
    pub best_bid: Option<String>,
    pub best_ask: Option<String>,
}
```

All new fields are `Option<String>` with serde's default deserialization — missing fields become `None`, so this is backwards-compatible.

**Step 2: Verify it compiles**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Compiles successfully. No behavior change yet.

**Step 3: Commit**

```bash
git add controller/src/polymarket.rs
git commit -m "feat(poly): add size, best_bid, best_ask to PriceChangeItem

Polymarket API sends these fields on every price_change message but we
were discarding them. Now deserialized for use by the shadow book."
```

---

### Task 3: Rewrite `process_book()` to use `PolyBook`

**Files:**
- Modify: `controller/src/polymarket.rs:609-728` (process_book function)

**Prerequisite docs:** Read design doc section "Step 3" and edge case 5d (size semantics).

**Step 1: Write the failing integration test**

Add to end of `controller/tests/integration_tests.rs`:

```rust
// ============================================================================
// POLYMARKET SHADOW BOOK TESTS - Verifies PolyBook correctly tracks depth
// and feeds the AtomicOrderbook cache, preventing phantom prices.
// See: https://github.com/Tydog99/Polymarket-Kalshi-Arbitrage-bot/issues/57
// ============================================================================

mod poly_shadow_book_tests {
    use arb_bot::types::*;
    use arb_bot::arb::ArbConfig;

    /// Helper: create a GlobalState with one market pair registered.
    /// Returns (state, market_id).
    fn setup_state() -> (GlobalState, u16) {
        let state = GlobalState::default();
        let pair = MarketPair {
            pair_id: "test-poly-book".into(),
            league: "nba".into(),
            market_type: MarketType::Moneyline,
            description: "Test Poly Shadow Book".into(),
            kalshi_event_ticker: "KXNBAGAME-TEST".into(),
            kalshi_market_ticker: "KXNBAGAME-TEST-YES".into(),
            kalshi_event_slug: "test-event".into(),
            poly_slug: "test-poly".into(),
            poly_yes_token: "yes_token_poly_book".into(),
            poly_no_token: "no_token_poly_book".into(),
            line_value: None,
            team_suffix: None,
            neg_risk: false,
        };
        let id = state.add_pair(pair).expect("Should add pair");
        // Set Kalshi side so arb detection has complete data
        state.markets[id as usize].kalshi.store(50, 50, 1000, 1000);
        (state, id)
    }

    /// Snapshot populates PolyBook and AtomicOrderbook with correct best ask.
    #[test]
    fn test_poly_snapshot_populates_book_and_cache() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Simulate snapshot: YES token asks at 45¢, 48¢, 52¢
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(48, 2000), (45, 1500), (52, 3000)]);
            let (price, size) = book.best_yes_ask().unwrap();
            // Drop lock before atomic update
            drop(book);
            market.poly.update_yes(price, size);
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 45, "Best YES ask should be lowest: 45¢");
        assert_eq!(yes_size, 1500, "Size should be from 45¢ level");
    }

    /// Snapshot resets book after price_change deltas were applied.
    #[test]
    fn test_poly_snapshot_resets_after_deltas() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Initial snapshot
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(45, 1000)]);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }

        // Delta: add level at 42¢
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(42, 500);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }
        assert_eq!(market.poly.load().0, 42, "Delta should have set best to 42¢");

        // New snapshot with completely different levels — replaces everything
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(60, 800), (65, 1200)]);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }
        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 60, "Snapshot should fully replace: best=60¢");
        assert_eq!(yes_size, 800, "Size from new snapshot's best level");
    }
}
```

**Step 2: Run the test to verify it passes (these use PolyBook directly, should pass after Task 1)**

Run: `cargo test poly_shadow_book_tests -- --no-capture`
Expected: PASS — these tests use `PolyBook` directly (not the private `process_book` function).

**Step 3: Rewrite `process_book()` function**

Replace the body of `process_book()` at `controller/src/polymarket.rs:609-728`.

The key changes from the current implementation:
1. Parse all ask levels into a `Vec<(u16, u16)>` (same parsing, keep the vec)
2. For each YES/NO token lookup: lock `poly_book`, call `set_yes_asks`/`set_no_asks`, read `best_yes_ask`/`best_no_ask`, drop lock, then `update_yes`/`update_no`
3. If `best_*_ask()` returns `None`, write `(0, 0)` to clear the cache

Replace the function body (lines 618-727) with:

```rust
    let token_hash = fxhash_str(&book.asset_id);

    // Parse all ask levels (keep full depth for PolyBook)
    let ask_levels: Vec<(u16, u16)> = book.asks.iter()
        .filter_map(|l| {
            let price = parse_price(&l.price);
            let size = parse_size(&l.size);
            if price > 0 { Some((price, size)) } else { None }
        })
        .collect();

    // A token can be YES for one market AND NO for another (e.g., esports where
    // Kalshi has separate markets for each team winning the same match).
    // We must check BOTH lookups, not return early.

    let mut matched = false;

    // Check if YES token
    let yes_market_id = state.poly_yes_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = yes_market_id {
        let market = &state.markets[market_id as usize];
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        // Populate PolyBook and derive best ask
        let (best_ask, ask_size) = {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&ask_levels);
            book.best_yes_ask().unwrap_or((0, 0))
        };

        tracing::debug!(
            "[POLY-SNAP] {} | YES asks: {:?} | best_yes_ask: {}¢ size={}",
            ticker, ask_levels, best_ask, ask_size
        );

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        market.poly.update_yes(best_ask, ask_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        // Check arbs using ArbOpportunity::detect()
        if let Some(req) = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            clock.now_ns(),
        ) {
            route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
        }
        matched = true;
    }

    // Check if NO token (same token can be NO for a different market)
    let no_market_id = state.poly_no_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = no_market_id {
        let market = &state.markets[market_id as usize];
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        // Populate PolyBook and derive best ask
        let (best_ask, ask_size) = {
            let mut book = market.poly_book.lock();
            book.set_no_asks(&ask_levels);
            book.best_no_ask().unwrap_or((0, 0))
        };

        tracing::debug!(
            "[POLY-SNAP] {} | NO asks: {:?} | best_no_ask: {}¢ size={}",
            ticker, ask_levels, best_ask, ask_size
        );

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        market.poly.update_no(best_ask, ask_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        // Check arbs using ArbOpportunity::detect()
        if let Some(req) = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            clock.now_ns(),
        ) {
            route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
        }
        matched = true;
    }

    if !matched {
        tracing::debug!(
            "[POLY] UNMATCHED token: asset={}...",
            &book.asset_id[..book.asset_id.len().min(20)]
        );
    }
```

**Step 4: Verify it compiles and tests pass**

Run: `cargo build --release 2>&1 | tail -5`
Run: `cargo test poly_shadow_book_tests -- --no-capture`
Run: `cargo test -- --no-capture 2>&1 | tail -20`
Expected: All tests PASS. No regressions.

**Step 5: Commit**

```bash
git add controller/src/polymarket.rs controller/tests/integration_tests.rs
git commit -m "feat(poly): rewrite process_book() to populate PolyBook shadow orderbook

Snapshots now store all ask levels in PolyBook and derive the best ask
from it, instead of extracting min and discarding depth. Fixes #57 bug 5."
```

---

### Task 4: Rewrite `process_price_change()` to use `PolyBook`

**Files:**
- Modify: `controller/src/polymarket.rs:732-843` (process_price_change function)
- Modify: `controller/tests/integration_tests.rs` (add more tests to poly_shadow_book_tests)

**Prerequisite docs:** Read design doc sections "Step 4" and edge cases 5a, 5e.

**Step 1: Write the failing integration tests**

Add to the `poly_shadow_book_tests` module in `controller/tests/integration_tests.rs`:

```rust
    /// price_change with size updates both PolyBook and AtomicOrderbook.
    #[test]
    fn test_poly_price_change_updates_book() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Initial snapshot: YES ask at 50¢
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(50, 1000)]);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }

        // price_change: new SELL level at 48¢ with size 500
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(48, 500);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 48, "New lower ask should become best");
        assert_eq!(yes_size, 500, "Size should be from new level");
    }

    /// Size from price_change is used, not stale snapshot size.
    #[test]
    fn test_poly_size_updates_from_price_change() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot: YES ask at 45¢ with size 1000
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(45, 1000)]);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }
        assert_eq!(market.poly.load().2, 1000);

        // price_change: same price 45¢ but size updated to 2500
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(45, 2500);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 45, "Price unchanged");
        assert_eq!(yes_size, 2500, "Size should be updated from price_change, not stale");
    }

    /// Removing best ask via size=0 reveals next-best level.
    #[test]
    fn test_poly_best_ask_removed_reveals_next() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot: asks at 45¢ and 50¢
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(45, 1000), (50, 2000)]);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }
        assert_eq!(market.poly.load().0, 45);

        // price_change: remove 45¢ level (size=0)
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(45, 0);
            let best = book.best_yes_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_yes(best.0, best.1);
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 50, "Next-best level (50¢) should be promoted");
        assert_eq!(yes_size, 2000, "Size from 50¢ level");
    }

    /// End-to-end: arb exists, then best ask is removed, arb disappears.
    #[test]
    fn test_poly_no_phantom_arb_after_removal() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];
        let config = ArbConfig::new(99, 1.0);

        // Kalshi: NO ask = 50¢
        market.kalshi.store(50, 50, 1000, 1000);

        // Poly: YES ask = 45¢ (cost = 45 + 50 = 95 < 99 threshold → arb exists)
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(45, 1000)]);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }

        let arb = ArbOpportunity::detect(
            id, market.kalshi.load(), market.poly.load(), &config, 0,
        );
        assert!(arb.is_some(), "Arb should exist: 45 + 50 = 95 < 99");

        // Now remove the 45¢ level — no other levels exist
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(45, 0);
            let best = book.best_yes_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_yes(best.0, best.1);
        }

        let arb = ArbOpportunity::detect(
            id, market.kalshi.load(), market.poly.load(), &config, 0,
        );
        assert!(arb.is_none(), "Arb should NOT exist: price=0 means no liquidity");
    }

    /// All levels removed → cache shows (0, 0) → arb detection skips.
    #[test]
    fn test_poly_empty_book_clears_cache() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot with two levels
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(45, 1000), (50, 2000)]);
            let (p, s) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(p, s);
        }

        // Remove both levels
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(45, 0);
            book.update_yes_level(50, 0);
            let best = book.best_yes_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_yes(best.0, best.1);
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 0, "Empty book → price 0");
        assert_eq!(yes_size, 0, "Empty book → size 0");
    }
```

**Step 2: Run new tests to verify they pass (they use PolyBook directly)**

Run: `cargo test poly_shadow_book_tests -- --no-capture`
Expected: All tests PASS.

**Step 3: Rewrite `process_price_change()`**

Replace the entire body of `process_price_change()` at `controller/src/polymarket.rs:732-843`.

Key changes from current implementation:
1. Parse `size` from the message (not just price)
2. Process SELL side through `PolyBook` instead of direct cache write
3. For BUY side: use `best_ask` field for sanity checking only
4. Remove the improvement-only arb filter — always check arbs
5. Handle `None` from `best_*_ask()` by writing `(0, 0)` to cache

Replace the function body (lines 741-843) with:

```rust
    let Some(price_str) = &change.price else { return };
    let price = parse_price(price_str);
    if price == 0 { return; }

    let is_sell = matches!(change.side.as_deref(), Some("SELL" | "sell"));

    // Parse size (absolute replacement — 0 means remove level)
    let size = change.size.as_deref()
        .map(|s| parse_size(s))
        .unwrap_or(0);

    // For BUY side: only use best_ask for sanity checking, don't update book
    if !is_sell {
        // Sanity check: if best_ask is present, compare with our cached value
        if let Some(api_best_ask_str) = &change.best_ask {
            let api_best_ask = parse_price(api_best_ask_str);
            if api_best_ask > 0 {
                let token_hash = fxhash_str(&change.asset_id);
                if let Some(market_id) = state.poly_yes_to_id.read().get(&token_hash).copied() {
                    let (our_best, _, _, _) = state.markets[market_id as usize].poly.load();
                    if our_best > 0 && our_best != api_best_ask {
                        let ticker = state.markets[market_id as usize].pair()
                            .map(|p| p.kalshi_market_ticker.to_string())
                            .unwrap_or_else(|| format!("market_{}", market_id));
                        tracing::warn!(
                            "[POLY] book drift: computed best_yes_ask={}¢ but API says best_ask={}¢ for {}",
                            our_best, api_best_ask, ticker
                        );
                    }
                }
            }
        }
        return;
    }

    // SELL side: apply to PolyBook and update AtomicOrderbook
    let token_hash = fxhash_str(&change.asset_id);

    // A token can be YES for one market AND NO for another (e.g., esports).
    // We must check BOTH lookups, not return early.

    // Check YES token
    let yes_market_id = state.poly_yes_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = yes_market_id {
        let market = &state.markets[market_id as usize];
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        let (prev_yes, _, _, _) = market.poly.load();

        // Apply to PolyBook and derive best ask
        let (best_ask, best_size) = {
            let mut book = market.poly_book.lock();
            book.update_yes_level(price, size);
            book.best_yes_ask().unwrap_or((0, 0))
        };

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        market.poly.update_yes(best_ask, best_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        tracing::debug!(
            "[POLY-DELTA] {} | YES: level {}¢ size={} → best_ask={}¢ (was {}¢)",
            ticker, price, size, best_ask, prev_yes
        );

        // Sanity check: compare our computed best_ask against API-reported best_ask
        if let Some(api_best_str) = &change.best_ask {
            let api_best = parse_price(api_best_str);
            if api_best > 0 && best_ask > 0 && best_ask != api_best {
                tracing::warn!(
                    "[POLY] book drift: computed best_yes_ask={}¢ but API says best_ask={}¢ for {}",
                    best_ask, api_best, ticker
                );
            }
        }

        // Always check arbs (removed improvement-only filter)
        if let Some(req) = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            clock.now_ns(),
        ) {
            route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
        }
    }

    // Check NO token (same token can be NO for a different market)
    let no_market_id = state.poly_no_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = no_market_id {
        let market = &state.markets[market_id as usize];
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        let (_, prev_no, _, _) = market.poly.load();

        // Apply to PolyBook and derive best ask
        let (best_ask, best_size) = {
            let mut book = market.poly_book.lock();
            book.update_no_level(price, size);
            book.best_no_ask().unwrap_or((0, 0))
        };

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        market.poly.update_no(best_ask, best_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        tracing::debug!(
            "[POLY-DELTA] {} | NO: level {}¢ size={} → best_ask={}¢ (was {}¢)",
            ticker, price, size, best_ask, prev_no
        );

        // Sanity check
        if let Some(api_best_str) = &change.best_ask {
            let api_best = parse_price(api_best_str);
            if api_best > 0 && best_ask > 0 && best_ask != api_best {
                tracing::warn!(
                    "[POLY] book drift: computed best_no_ask={}¢ but API says best_ask={}¢ for {}",
                    best_ask, api_best, ticker
                );
            }
        }

        // Always check arbs (removed improvement-only filter)
        if let Some(req) = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            clock.now_ns(),
        ) {
            route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
        }
    }
```

**Step 4: Verify it compiles and all tests pass**

Run: `cargo build --release 2>&1 | tail -5`
Run: `cargo test -- --no-capture 2>&1 | tail -20`
Expected: All tests PASS. No regressions.

**Step 5: Commit**

```bash
git add controller/src/polymarket.rs controller/tests/integration_tests.rs
git commit -m "feat(poly): rewrite process_price_change() to use PolyBook shadow orderbook

- Parse size field from price_change messages (was silently discarded)
- Apply SELL updates to PolyBook, derive best ask from full depth
- Remove improvement-only arb filter (always check after book update)
- Add best_ask sanity check logging for drift detection
- BUY side: sanity check only (no book update needed)

Fixes #57 bugs 1-4: phantom prices, stale sizes, missed arb
invalidations, discarded BUY-side validation data."
```

---

### Task 5: Final verification and cleanup

**Files:**
- Check: `controller/src/polymarket.rs` (remove `#[allow(dead_code)]` from `bids` if we want)
- Check: `controller/src/types.rs`
- Check: `controller/tests/integration_tests.rs`

**Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -20`
Expected: All tests PASS.

**Step 2: Build release and check for warnings**

Run: `cargo build --release 2>&1 | grep -E "warning|error" | head -20`
Expected: No new warnings from our changes. (Pre-existing warnings in other files are acceptable.)

**Step 3: Remove `#[allow(dead_code)]` from `bids` in `BookSnapshot` if still present**

Check `controller/src/polymarket.rs:29`. The `bids` field on `BookSnapshot` is still `#[allow(dead_code)]` — we still don't use bids from book snapshots (we only track asks). Leave it as-is unless the compiler warns.

**Step 4: Verify the PolyBook tests specifically**

Run: `cargo test poly_book -- --no-capture`
Run: `cargo test poly_shadow_book -- --no-capture`
Expected: All PASS.

**Step 5: Commit any cleanup**

If any cleanup was needed:
```bash
git add -A
git commit -m "chore: cleanup warnings from poly shadow book implementation"
```

---

## Summary of all changes

| File | What changed |
|------|-------------|
| `controller/src/types.rs:7,11` | Import `BTreeMap`, `Mutex` |
| `controller/src/types.rs:163+` | Add `PolyBook` struct + impl (10 methods) |
| `controller/src/types.rs:165+` | Add `poly_book: Mutex<PolyBook>` to `AtomicMarketState` |
| `controller/src/types.rs:626+` | Add 10 unit tests for `PolyBook` |
| `controller/src/polymarket.rs:49-54` | Add `size`, `best_bid`, `best_ask` to `PriceChangeItem` |
| `controller/src/polymarket.rs:609-728` | Rewrite `process_book()` to use `PolyBook` |
| `controller/src/polymarket.rs:732-843` | Rewrite `process_price_change()` to use `PolyBook` |
| `controller/tests/integration_tests.rs` | Add 7 integration tests in `poly_shadow_book_tests` |

## What is NOT changed

- `AtomicOrderbook` layout (packed u64)
- `ArbOpportunity::detect()` logic
- Execution flow (`execution.rs`)
- Confirm queue validation (`confirm_queue.rs`)
- WebSocket subscription/reconnection logic
- KalshiBook (separate issue #56)
