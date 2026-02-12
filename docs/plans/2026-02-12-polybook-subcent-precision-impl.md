# PolyBook Sub-Cent Precision Fix — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix PolyBook depth loss caused by sub-cent Polymarket prices colliding at whole-cent BTreeMap keys.

**Architecture:** Change PolyBook from `BTreeMap<u16, u16>` (cents) to `BTreeMap<u32, u32>` (milli-cents). Add `parse_price_millis` and `parse_size_millis` for sub-cent parsing. Convert back to cents at the PolyBook→AtomicOrderbook boundary. No changes to AtomicOrderbook, ArbOpportunity, or execution.

**Tech Stack:** Rust, BTreeMap, u32 arithmetic

---

### Task 1: Add milli-cent types and parse functions

**Files:**
- Modify: `controller/src/types.rs:74-77` (add new types after existing PriceCents/SizeCents)
- Modify: `controller/src/types.rs:496-517` (add new parse functions after existing parse_price)

**Step 1: Write failing tests for parse_price_millis**

Add to the `#[cfg(test)] mod tests` block in `controller/src/types.rs`, before the PolyBook tests section:

```rust
    // =========================================================================
    // Milli-cent Parse Tests
    // =========================================================================

    #[test]
    fn test_parse_price_millis_two_decimals() {
        assert_eq!(parse_price_millis("0.99"), 990);
        assert_eq!(parse_price_millis("0.05"), 50);
        assert_eq!(parse_price_millis("0.50"), 500);
        assert_eq!(parse_price_millis("0.01"), 10);
    }

    #[test]
    fn test_parse_price_millis_three_decimals() {
        assert_eq!(parse_price_millis("0.999"), 999);
        assert_eq!(parse_price_millis("0.998"), 998);
        assert_eq!(parse_price_millis("0.001"), 1);
        assert_eq!(parse_price_millis("0.049"), 49);
        assert_eq!(parse_price_millis("0.988"), 988);
    }

    #[test]
    fn test_parse_price_millis_one_decimal() {
        assert_eq!(parse_price_millis("0.5"), 500);
        assert_eq!(parse_price_millis("0.1"), 100);
        assert_eq!(parse_price_millis("0.9"), 900);
    }

    #[test]
    fn test_parse_price_millis_invalid() {
        assert_eq!(parse_price_millis(""), 0);
        assert_eq!(parse_price_millis("abc"), 0);
        assert_eq!(parse_price_millis("1.5"), 0); // > 1.0, invalid for prediction market
    }

    #[test]
    fn test_parse_size_millis_integer() {
        assert_eq!(parse_size_millis("100"), 100_000);
        assert_eq!(parse_size_millis("1"), 1_000);
    }

    #[test]
    fn test_parse_size_millis_fractional() {
        assert_eq!(parse_size_millis("123.45"), 123_450);
        assert_eq!(parse_size_millis("260323.77"), 260_323_770);
        assert_eq!(parse_size_millis("0.5"), 500);
    }

    #[test]
    fn test_parse_size_millis_invalid() {
        assert_eq!(parse_size_millis(""), 0);
        assert_eq!(parse_size_millis("abc"), 0);
    }

    #[test]
    fn test_millis_to_cents() {
        assert_eq!(millis_to_cents(990), 99);  // 0.99 → 99¢
        assert_eq!(millis_to_cents(999), 99);  // 0.999 → 99¢ (floor)
        assert_eq!(millis_to_cents(50), 5);    // 0.05 → 5¢
        assert_eq!(millis_to_cents(49), 4);    // 0.049 → 4¢ (floor)
        assert_eq!(millis_to_cents(0), 0);     // no price
        assert_eq!(millis_to_cents(10), 1);    // 0.01 → 1¢
        assert_eq!(millis_to_cents(1), 0);     // 0.001 → 0¢ (below 1¢)
    }

    #[test]
    fn test_millis_size_to_cents() {
        assert_eq!(millis_size_to_cents(100_000), 10_000);   // $100 → 10000¢
        assert_eq!(millis_size_to_cents(123_450), 12_345);   // $123.45 → 12345¢
        assert_eq!(millis_size_to_cents(700_000), 65_535);   // $700 → clamped to u16::MAX
        assert_eq!(millis_size_to_cents(0), 0);
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib -- test_parse_price_millis 2>&1 | tail -5`
Expected: compilation errors — functions don't exist yet

**Step 3: Add types and implement parse functions**

In `controller/src/types.rs` after line 77 (`pub type SizeCents = u16;`), add:

```rust
/// Price in milli-cents for Polymarket sub-cent precision (1 unit = $0.001). Range: 0–999.
pub type PolyPriceMillis = u32;

/// Size in milli-dollars for Polymarket (1 unit = $0.001). Handles large sizes like 260323.77.
pub type PolySizeMillis = u32;
```

After the existing `parse_price` function (after line 517), add:

```rust
/// Parse Polymarket price string to milli-cents (1 unit = $0.001).
/// "0.999" → 999, "0.99" → 990, "0.05" → 50, "0.5" → 500
/// Returns 0 if parsing fails or price >= 1.0.
#[inline(always)]
pub fn parse_price_millis(s: &str) -> PolyPriceMillis {
    let val: f64 = match s.parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if val <= 0.0 || val >= 1.0 {
        return 0;
    }
    (val * 1000.0).round() as PolyPriceMillis
}

/// Parse Polymarket size string to milli-dollars (1 unit = $0.001).
/// "260323.77" → 260_323_770, "100" → 100_000
/// Returns 0 if parsing fails.
#[inline(always)]
pub fn parse_size_millis(s: &str) -> PolySizeMillis {
    s.parse::<f64>()
        .map(|size| (size * 1000.0).round() as PolySizeMillis)
        .unwrap_or(0)
}

/// Convert milli-cent price to whole cents (floor division).
/// 999 → 99, 990 → 99, 49 → 4, 0 → 0
#[inline(always)]
pub fn millis_to_cents(millis: PolyPriceMillis) -> PriceCents {
    (millis / 10).min(99) as PriceCents
}

/// Convert milli-dollar size to cents, clamped to u16::MAX.
/// 100_000 → 10_000, 700_000 → 65_535
#[inline(always)]
pub fn millis_size_to_cents(millis: PolySizeMillis) -> SizeCents {
    (millis / 10).min(u16::MAX as u32) as SizeCents
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib -- test_parse_price_millis test_parse_size_millis test_millis_to_cents test_millis_size_to_cents -v 2>&1 | tail -20`
Expected: all 8 tests PASS

**Step 5: Commit**

```bash
git add controller/src/types.rs
git commit -m "feat(types): add milli-cent types and parse functions for sub-cent Polymarket prices"
```

---

### Task 2: Change PolyBook to use milli-cent storage

**Files:**
- Modify: `controller/src/types.rs:310-382` (PolyBook struct and impl)
- Modify: `controller/src/types.rs:2391-2475` (PolyBook unit tests)

**Step 1: Update PolyBook unit tests to use milli-cent values**

Replace the entire PolyBook tests section (`test_poly_book_new_is_empty` through `test_poly_book_clear`) with milli-cent equivalents. Values change: old `45` (cents) → new `450` (millis), old `1000` (size in cents) → new `10000` (size in millis).

```rust
    // =========================================================================
    // PolyBook Tests - Shadow orderbook for Polymarket (milli-cent precision)
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
        // 0.47, 0.45, 0.50 in millis
        book.set_yes_asks(&[(470, 20000), (450, 15000), (500, 30000)]);
        assert_eq!(book.best_yes_ask(), Some((450, 15000)));
    }

    #[test]
    fn test_poly_book_update_level_insert() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(500, 10000)]);
        book.update_yes_level(480, 5000);
        assert_eq!(book.best_yes_ask(), Some((480, 5000)));
    }

    #[test]
    fn test_poly_book_update_level_replace() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(450, 10000)]);
        book.update_yes_level(450, 20000);
        assert_eq!(book.best_yes_ask(), Some((450, 20000)));
    }

    #[test]
    fn test_poly_book_update_level_remove() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(450, 10000), (500, 20000)]);
        book.update_yes_level(450, 0);
        assert_eq!(book.best_yes_ask(), Some((500, 20000)));
    }

    #[test]
    fn test_poly_book_remove_best_reveals_next() {
        let mut book = PolyBook::new();
        book.set_no_asks(&[(420, 5000), (450, 10000), (480, 15000)]);
        assert_eq!(book.best_no_ask(), Some((420, 5000)));
        book.update_no_level(420, 0);
        assert_eq!(book.best_no_ask(), Some((450, 10000)));
    }

    #[test]
    fn test_poly_book_remove_last_level() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(500, 10000)]);
        book.update_yes_level(500, 0);
        assert_eq!(book.best_yes_ask(), None);
    }

    #[test]
    fn test_poly_book_snapshot_replaces_all() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(450, 10000), (500, 20000)]);
        book.set_yes_asks(&[(600, 5000), (650, 8000)]);
        assert_eq!(book.best_yes_ask(), Some((600, 5000)));
        book.update_yes_level(450, 0); // old level, no-op
        assert_eq!(book.best_yes_ask(), Some((600, 5000)));
    }

    #[test]
    fn test_poly_book_skips_zero_price_and_size() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(0, 10000), (450, 0), (500, 20000)]);
        assert_eq!(book.best_yes_ask(), Some((500, 20000)));
    }

    #[test]
    fn test_poly_book_clear() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(450, 10000)]);
        book.set_no_asks(&[(550, 20000)]);
        book.clear();
        assert_eq!(book.best_yes_ask(), None);
        assert_eq!(book.best_no_ask(), None);
    }

    // --- Sub-cent precision tests (the actual bug fix) ---

    #[test]
    fn test_poly_book_subcent_prices_are_unique() {
        let mut book = PolyBook::new();
        // 0.999, 0.998, 0.99 — previously all mapped to 99¢
        book.set_yes_asks(&[(999, 100000), (998, 55000), (990, 92075)]);
        // All three levels should exist
        assert_eq!(book.best_yes_ask(), Some((990, 92075))); // lowest price
    }

    #[test]
    fn test_poly_book_remove_subcent_preserves_others() {
        let mut book = PolyBook::new();
        // Three levels near 97¢: 0.97, 0.978, 0.979
        book.set_no_asks(&[(970, 10000), (978, 5000), (979, 12900)]);
        assert_eq!(book.best_no_ask(), Some((970, 10000)));

        // Remove 0.97 level — 0.978 should become best
        book.update_no_level(970, 0);
        assert_eq!(book.best_no_ask(), Some((978, 5000)));
    }

    #[test]
    fn test_poly_book_subcent_delta_does_not_affect_neighbors() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(50, 8500), (49, 2000)]);  // 0.05 and 0.049
        assert_eq!(book.best_yes_ask(), Some((49, 2000)));

        // Remove 0.049 — 0.05 should survive
        book.update_yes_level(49, 0);
        assert_eq!(book.best_yes_ask(), Some((50, 8500)));
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib -- test_poly_book 2>&1 | tail -5`
Expected: compilation errors — PolyBook still uses u16

**Step 3: Update PolyBook struct and impl to use u32**

Replace the PolyBook struct and impl in `controller/src/types.rs:310-382`:

```rust
/// Shadow orderbook for Polymarket.
///
/// Maintains full ask-side depth for YES and NO tokens so the true best ask
/// is always derivable — even after the current best is removed.
///
/// Uses milli-cent precision (u32 keys, 1 unit = $0.001) to avoid collisions
/// when Polymarket sends sub-cent prices like 0.999 and 0.998.
///
/// Polymarket sends asks directly (unlike Kalshi bids). Deltas use absolute
/// replacement (`qty = new_size`), not incremental (`qty += delta`).
/// `size = 0` means remove the level.
pub struct PolyBook {
    yes_asks: BTreeMap<PolyPriceMillis, PolySizeMillis>,
    no_asks: BTreeMap<PolyPriceMillis, PolySizeMillis>,
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
    pub fn set_yes_asks(&mut self, levels: &[(PolyPriceMillis, PolySizeMillis)]) {
        self.yes_asks.clear();
        for &(price, size) in levels {
            if price > 0 && size > 0 {
                self.yes_asks.insert(price, size);
            }
        }
    }

    /// Replace all NO ask levels from a book snapshot.
    pub fn set_no_asks(&mut self, levels: &[(PolyPriceMillis, PolySizeMillis)]) {
        self.no_asks.clear();
        for &(price, size) in levels {
            if price > 0 && size > 0 {
                self.no_asks.insert(price, size);
            }
        }
    }

    /// Apply a YES ask level update (absolute replacement).
    /// size = 0 removes the level.
    pub fn update_yes_level(&mut self, price: PolyPriceMillis, size: PolySizeMillis) {
        if size == 0 {
            self.yes_asks.remove(&price);
        } else {
            self.yes_asks.insert(price, size);
        }
    }

    /// Apply a NO ask level update (absolute replacement).
    pub fn update_no_level(&mut self, price: PolyPriceMillis, size: PolySizeMillis) {
        if size == 0 {
            self.no_asks.remove(&price);
        } else {
            self.no_asks.insert(price, size);
        }
    }

    /// Best YES ask: lowest price in the book (milli-cent precision).
    /// BTreeMap is ascending, so `iter().next()` is the lowest key.
    pub fn best_yes_ask(&self) -> Option<(PolyPriceMillis, PolySizeMillis)> {
        self.yes_asks.iter().next().map(|(&p, &s)| (p, s))
    }

    /// Best NO ask: lowest price in the book (milli-cent precision).
    pub fn best_no_ask(&self) -> Option<(PolyPriceMillis, PolySizeMillis)> {
        self.no_asks.iter().next().map(|(&p, &s)| (p, s))
    }

    /// Clear all state.
    pub fn clear(&mut self) {
        self.yes_asks.clear();
        self.no_asks.clear();
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib -- test_poly_book 2>&1 | tail -20`
Expected: all 14 PolyBook tests PASS (11 updated + 3 new sub-cent tests)

**Step 5: Commit**

```bash
git add controller/src/types.rs
git commit -m "fix(types): change PolyBook to milli-cent precision to prevent sub-cent price collisions"
```

---

### Task 3: Update polymarket.rs callers to use milli-cent parsing and boundary conversion

**Files:**
- Modify: `controller/src/polymarket.rs:18-21` (imports)
- Modify: `controller/src/polymarket.rs:299-306` (parse_size — keep for BUY-side, but add parse_size_millis)
- Modify: `controller/src/polymarket.rs:624-630` (process_book snapshot parsing)
- Modify: `controller/src/polymarket.rs:646-662` (process_book YES → AtomicOrderbook)
- Modify: `controller/src/polymarket.rs:692-708` (process_book NO → AtomicOrderbook)
- Modify: `controller/src/polymarket.rs:749-758` (process_price_change parse)
- Modify: `controller/src/polymarket.rs:826-837` (process_price_change YES → AtomicOrderbook)
- Modify: `controller/src/polymarket.rs:885-896` (process_price_change NO → AtomicOrderbook)

**Step 1: Update imports**

In `controller/src/polymarket.rs:18-21`, add the new functions to the import:

```rust
use crate::types::{
    GlobalState, ArbOpportunity, MarketPair, PriceCents, SizeCents,
    parse_price, parse_price_millis, parse_size_millis, millis_to_cents, millis_size_to_cents,
    fxhash_str,
};
```

**Step 2: Update process_book snapshot parsing (line 624-630)**

Change from:
```rust
    let ask_levels: Vec<(u16, u16)> = book.asks.iter()
        .filter_map(|l| {
            let price = parse_price(&l.price);
            let size = parse_size(&l.size);
            if price > 0 { Some((price, size)) } else { None }
        })
        .collect();
```

To:
```rust
    let ask_levels: Vec<(u32, u32)> = book.asks.iter()
        .filter_map(|l| {
            let price = parse_price_millis(&l.price);
            let size = parse_size_millis(&l.size);
            if price > 0 { Some((price, size)) } else { None }
        })
        .collect();
```

**Step 3: Update process_book YES boundary conversion (line 646-662)**

Change from:
```rust
        let (best_ask, ask_size) = {
            let mut pbook = market.poly_book.lock();
            pbook.set_yes_asks(&ask_levels);
            pbook.best_yes_ask().unwrap_or((0, 0))
        };
        ...
        market.poly.update_yes(best_ask, ask_size);
```

To:
```rust
        let (best_millis, size_millis) = {
            let mut pbook = market.poly_book.lock();
            pbook.set_yes_asks(&ask_levels);
            pbook.best_yes_ask().unwrap_or((0, 0))
        };
        let best_ask = millis_to_cents(best_millis);
        let ask_size = millis_size_to_cents(size_millis);
        ...
        market.poly.update_yes(best_ask, ask_size);
```

Update the debug log to show both millis and cents:
```rust
        tracing::debug!(
            "[POLY-SNAP] {} | YES asks: {} levels | best_yes_ask: {}m ({}¢) size={}",
            ticker, ask_levels.len(), best_millis, best_ask, ask_size
        );
```

**Step 4: Update process_book NO boundary conversion (line 692-708)**

Same pattern as YES side:
```rust
        let (best_millis, size_millis) = {
            let mut pbook = market.poly_book.lock();
            pbook.set_no_asks(&ask_levels);
            pbook.best_no_ask().unwrap_or((0, 0))
        };
        let best_ask = millis_to_cents(best_millis);
        let ask_size = millis_size_to_cents(size_millis);
        ...
        market.poly.update_no(best_ask, ask_size);
```

Update the debug log similarly.

**Step 5: Update process_price_change parsing (line 749-758)**

Change the price/size parsing for SELL-side from:
```rust
    let price = parse_price(price_str);
    if price == 0 { return; }
    ...
    let size = change.size.as_deref()
        .map(|s| parse_size(s))
        .unwrap_or(0);
```

To:
```rust
    let price_millis = parse_price_millis(price_str);
    if price_millis == 0 { return; }

    let is_sell = matches!(change.side.as_deref(), Some("SELL" | "sell"));

    let size_millis = change.size.as_deref()
        .map(|s| parse_size_millis(s))
        .unwrap_or(0);
```

Keep a cents version for the BUY-side sanity check (which still compares against AtomicOrderbook in cents):
```rust
    // BUY side still uses cents for sanity check against AtomicOrderbook
    if !is_sell {
        // ... existing BUY side logic unchanged, still uses parse_price for api_best_ask ...
        return;
    }
```

**Step 6: Update process_price_change YES SELL boundary (line 826-837)**

Change from:
```rust
        let (best_ask, best_size) = {
            let mut pbook = market.poly_book.lock();
            pbook.update_yes_level(price, size);
            pbook.best_yes_ask().unwrap_or((0, 0))
        };
        ...
        market.poly.update_yes(best_ask, best_size);
```

To:
```rust
        let (best_millis, size_millis) = {
            let mut pbook = market.poly_book.lock();
            pbook.update_yes_level(price_millis, size_millis);
            pbook.best_yes_ask().unwrap_or((0, 0))
        };
        let best_ask = millis_to_cents(best_millis);
        let best_size = millis_size_to_cents(size_millis);
        ...
        market.poly.update_yes(best_ask, best_size);
```

Update debug log:
```rust
        tracing::debug!(
            "[POLY-DELTA] {} | YES: level {}m size={} → best_ask={}m ({}¢) (was {}¢)",
            ticker, price_millis, size_millis, best_millis, best_ask, prev_yes
        );
```

**Step 7: Update process_price_change NO SELL boundary (line 885-896)**

Same pattern as YES side.

**Step 8: Verify compilation**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles with only pre-existing warnings

**Step 9: Commit**

```bash
git add controller/src/polymarket.rs
git commit -m "fix(poly): use milli-cent parsing for PolyBook snapshots and deltas"
```

---

### Task 4: Update integration tests for milli-cent PolyBook

**Files:**
- Modify: `controller/tests/integration_tests.rs:2720-2972` (poly_shadow_book_tests module)

**Step 1: Update all integration tests**

All `poly_shadow_book_tests` calls to `set_yes_asks`, `set_no_asks`, `update_yes_level`, `update_no_level` need milli-cent values. The boundary conversion (PolyBook → AtomicOrderbook) now uses `millis_to_cents` / `millis_size_to_cents`.

Update the helper pattern from:
```rust
let (p, s) = book.best_yes_ask().unwrap();
drop(book);
market.poly.update_yes(p, s);
```

To:
```rust
let (p_millis, s_millis) = book.best_yes_ask().unwrap();
drop(book);
market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
```

Values to update (old cents → new millis):
- `(45, 1000)` → `(450, 10000)` (0.045 at $10.00)
- `(48, 2000)` → `(480, 20000)`
- `(50, 2000)` → `(500, 20000)`
- `(52, 3000)` → `(520, 30000)`
- `(42, 500)` → `(420, 5000)`
- `(55, 1000)` → `(550, 10000)`
- `(60, 800)` → `(600, 8000)`
- `(65, 1200)` → `(650, 12000)`

Add import at top of module:
```rust
use arb_bot::types::{millis_to_cents, millis_size_to_cents};
```

Also update AtomicOrderbook assertions: prices stay in cents (since `millis_to_cents(450) = 45`), sizes change from `millis_size_to_cents(10000) = 1000`.

**Step 2: Add sub-cent collision regression test**

Add a new test in `poly_shadow_book_tests`:

```rust
    /// Regression: sub-cent prices that previously collided at same cent value.
    /// API sends 0.999, 0.998, 0.99 — all mapped to 99¢ before fix.
    #[test]
    fn test_poly_subcent_no_collision() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot with sub-cent prices near 97¢
        {
            let mut book = market.poly_book.lock();
            book.set_no_asks(&[(970, 10000), (978, 5000), (979, 12900)]);
            let (p, s) = book.best_no_ask().unwrap();
            drop(book);
            market.poly.update_no(millis_to_cents(p), millis_size_to_cents(s));
        }
        assert_eq!(market.poly.load().1, 97); // 970 millis → 97¢

        // Remove 0.97 level — 0.978 should become best (not empty!)
        {
            let mut book = market.poly_book.lock();
            book.update_no_level(970, 0);
            let best = book.best_no_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_no(millis_to_cents(best.0), millis_size_to_cents(best.1));
        }
        // Before fix: NO ask would jump to 0 or 99 (lost depth). After fix: 97¢ from 0.978
        assert_eq!(market.poly.load().1, 97, "Sub-cent level at 0.978 should still show as 97¢");
    }
```

**Step 3: Run integration tests**

Run: `cargo test --test integration_tests -- poly_shadow 2>&1 | tail -20`
Expected: all tests PASS

**Step 4: Run full test suite**

Run: `cargo test 2>&1 | tail -10`
Expected: all tests PASS (except the 2 pre-existing position tracker failures)

**Step 5: Commit**

```bash
git add controller/tests/integration_tests.rs
git commit -m "test(poly): update integration tests for milli-cent PolyBook precision"
```

---

### Task 5: Live verification

**Step 1: Build release**

Run: `cargo build --release 2>&1 | tail -3`

**Step 2: Run against live market and verify drift is reduced**

Run: `RUST_LOG=controller::polymarket=debug,controller=info DRY_RUN=1 dotenvx run -- ./target/release/controller 2>&1 | tee /tmp/poly_millis_test.log`

Let it run for ~60 seconds, then check:
```bash
grep -c "book drift" /tmp/poly_millis_test.log
```

Expected: significantly fewer drift warnings than before (ideally zero for new data, though some transient drift from BUY-side timing is normal).

**Step 3: Commit any fixes if needed**
