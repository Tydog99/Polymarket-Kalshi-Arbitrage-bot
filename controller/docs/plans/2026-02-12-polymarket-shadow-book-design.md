# Fix Polymarket Shadow Orderbook (Issue #57)

## Context

The Polymarket orderbook handler has the **same class of bug** fixed for Kalshi in #56: no shadow book means stale/phantom prices that cause false arb detection, one-legged fills, and unhedged exposure.

Currently, `process_price_change()` (`polymarket.rs:772`) blindly overwrites the `AtomicOrderbook` cache with whatever price arrives. When the best ask is removed, there is no underlying book to fall back to — the cache holds a phantom price until the next full book snapshot arrives. Additionally, the `size` field sent by the Polymarket API is not even deserialized, so sizes are frozen from the last snapshot forever.

### Confirmed API semantics (verified against [Polymarket docs](https://docs.polymarket.com/developers/CLOB/websocket/market-channel))

| Property | Kalshi | Polymarket |
|----------|--------|------------|
| What's sent | Bids (asks derived via `100 - bid`) | Asks directly |
| Delta semantics | **Incremental** (`qty += delta`) | **Absolute replacement** (`qty = new_size`) |
| Size = 0 means | N/A (negative delta removes) | **Remove the level** |
| Book resyncs | Rare snapshots | Every trade triggers a full `book` event |
| Extra fields | None | `best_bid`, `best_ask` per price_change item |

Absolute replacement is simpler than Kalshi's incremental deltas — no risk of drift from missed deltas. Frequent book snapshots provide natural resync.

## The Five Bugs Being Fixed

### Bug 1: No shadow book — price changes blindly overwrite

`process_price_change()` at `polymarket.rs:772`:
```rust
market.poly.update_yes(price, current_yes_size);
```
Whatever `price` arrives becomes the cached ask. If the best ask at 45¢ is removed and a `price_change` for 47¢ arrives, the cache now says 47¢. If 47¢ is then also removed, the cache is stuck at 47¢ until the next snapshot.

### Bug 2: `size` field not parsed from `price_change` messages

`PriceChangeItem` (`polymarket.rs:50-54`) only parses `asset_id`, `price`, `side`. But the API sends `size`, `best_bid`, `best_ask`, and `hash` in every message:
```json
{
    "asset_id": "71321...",
    "price": "0.5",
    "size": "200",
    "side": "BUY",
    "hash": "56621a...",
    "best_bid": "0.5",
    "best_ask": "1"
}
```
Because `size` is never parsed, `process_price_change` passes `current_yes_size` (stale from last snapshot) into `update_yes()`. Size goes stale immediately.

### Bug 3: Arb check only fires on price improvement

`polymarket.rs:788`:
```rust
if price < current_yes || current_yes == 0 {
    // only then check for arbs
}
```
When the best ask is removed and the real best is now *higher*, no arb check fires — the cache silently holds a too-cheap phantom price. This is the exact failure mode that caused the Kalshi phantom arb incident.

### Bug 4: BUY side completely discarded

`polymarket.rs:743`:
```rust
if !matches!(change.side.as_deref(), Some("SELL" | "sell")) { return; }
```
All bid-side updates are thrown away. The API includes `best_bid` and `best_ask` in every `price_change` — free validation data, completely ignored.

### Bug 5: Snapshot discards all depth

`polymarket.rs:621-633`: The snapshot parses all ask levels, finds `min_by_key`, stores that single price+size, and discards everything. The `bids` field on `BookSnapshot` is `#[allow(dead_code)]` and never read.

## Files to Modify

1. `controller/src/types.rs` — Add `PolyBook` struct, add field to `AtomicMarketState`
2. `controller/src/polymarket.rs` — Add fields to `PriceChangeItem`, rewrite `process_book()` and `process_price_change()`
3. `controller/tests/integration_tests.rs` — Add correctness tests

## Step 1: Add `PolyBook` to `types.rs`

**1a.** Define `PolyBook` struct (after `AtomicOrderbook` impl, before `AtomicMarketState`):

```rust
/// Shadow orderbook for Polymarket.
///
/// Maintains full ask-side depth for YES and NO tokens so the true best ask
/// is always derivable — even after the current best is removed.
///
/// Unlike `KalshiBook` which tracks bids and derives asks (`100 - bid`),
/// Polymarket sends asks directly. And unlike Kalshi's incremental deltas
/// (`qty += delta`), Polymarket uses absolute replacement (`qty = new_size`,
/// `size = 0` means remove level).
pub struct PolyBook {
    /// YES token ask levels: price_cents → size_cents (absolute)
    yes_asks: BTreeMap<u16, u16>,
    /// NO token ask levels: price_cents → size_cents (absolute)
    no_asks: BTreeMap<u16, u16>,
}
```

**1b.** Methods on `PolyBook`:

```rust
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

    /// Clear all state (used on reconnect).
    pub fn clear(&mut self) {
        self.yes_asks.clear();
        self.no_asks.clear();
    }
}
```

**Why `BTreeMap<u16, u16>`:** Sorted ascending by price. `iter().next()` gives the lowest ask (best ask for a buyer) in O(log n). Books have ~5-20 levels. Using `u16` (matching `PriceCents`/`SizeCents`) means zero conversion at the `AtomicOrderbook` boundary. Memory: 1024 markets × 2 sides × 20 levels × 4 bytes = ~160KB worst case.

**Why asks, not bids:** The bot buys asks (it's buying YES or NO contracts). Polymarket sends ask levels directly — no derivation needed (unlike Kalshi where asks are derived from `100 - bid`). Bids are irrelevant for arb detection since we never sell.

**Why `Mutex` not `RwLock`:** Only the single Polymarket WebSocket thread(s) access the BTreeMap. No readers need it — arb detection reads `AtomicOrderbook` instead. The BTreeMap is an internal source of truth that feeds the lock-free atomic cache.

**1c.** Add field to `AtomicMarketState` (`types.rs:165`):

```rust
pub poly_book: Mutex<PolyBook>,
```

**1d.** Initialize in `AtomicMarketState::new()` (`types.rs:186`):

```rust
poly_book: Mutex::new(PolyBook::new()),
```

## Step 2: Add missing fields to `PriceChangeItem` (`polymarket.rs:50-54`)

```rust
#[derive(Deserialize, Debug)]
pub struct PriceChangeItem {
    pub asset_id: String,
    pub price: Option<String>,
    pub side: Option<String>,
    pub size: Option<String>,       // absolute size at this level
    pub best_bid: Option<String>,   // API-reported best bid (sanity check)
    pub best_ask: Option<String>,   // API-reported best ask (sanity check)
}
```

The `hash` field is intentionally omitted — we don't need order hashes.

## Step 3: Rewrite `process_book()` (`polymarket.rs:609-728`)

Make the snapshot handler populate the `PolyBook` first, then derive the `AtomicOrderbook` from it. This ensures snapshot and delta paths use the same source of truth.

**For each `BookSnapshot`:**

1. Parse all ask levels into `Vec<(u16, u16)>` (same as today, but keep the full vec)
2. Look up token in `poly_yes_to_id` / `poly_no_to_id`
3. Lock `market.poly_book`
4. Call `book.set_yes_asks(&levels)` or `book.set_no_asks(&levels)`
5. Read `book.best_yes_ask()` / `book.best_no_ask()` to get `(price, size)`
6. Drop the lock
7. `market.poly.update_yes(price, size)` or `market.poly.update_no(price, size)`
8. Run arb detection unconditionally (same as today)

**Key change:** Today's code calls `min_by_key` on a temporary vec and discards it. After this change, the full depth is stored in the `PolyBook` and the best ask is derived from it.

**Handling YES+NO dual lookup:** A single token can be YES for one market and NO for another (esports). The current dual-lookup pattern is preserved. Each market's `PolyBook` only stores the levels relevant to that market's YES or NO side. The parsed `levels` vec is shared across both lookups (it's the same set of asks — the token has one book regardless of what it represents to different markets).

## Step 4: Rewrite `process_price_change()` (`polymarket.rs:732-843`)

Complete rewrite to use `PolyBook` and parse the `size` field:

1. Parse `price` and `size` from the `PriceChangeItem`. Both are required — return early if either is missing.
2. Convert price via `parse_price()`, size via `parse_size()`.
3. Process **both** SELL and BUY sides (removing the SELL-only filter):
   - **SELL side updates** → these are ask-side changes. Apply to `PolyBook` and update `AtomicOrderbook`.
   - **BUY side updates** → log only. We don't need bids for arb detection, but the `best_bid`/`best_ask` fields are useful for sanity checking.

**For SELL side updates:**

4. Look up token in `poly_yes_to_id` / `poly_no_to_id` (same dual-lookup pattern)
5. Lock `market.poly_book`
6. Call `book.update_yes_level(price, size)` or `book.update_no_level(price, size)`
7. Read `book.best_yes_ask()` / `book.best_no_ask()` to get the current best
8. Drop the lock
9. If best is `Some((p, s))`: `market.poly.update_yes(p, s)` / `market.poly.update_no(p, s)`
   If best is `None` (book empty): `market.poly.update_yes(0, 0)` / `market.poly.update_no(0, 0)`
10. **Always run arb detection** — remove the improvement-only filter. The book handles level removal correctly, so we must check arbs on every update (a price worsening might invalidate a previously cached arb, and a price removal might reveal the next-best level which could create a new arb).

**Sanity check (debug builds only):**

11. If `best_ask` is present in the message, compare against our computed best ask. Log at `warn` level on mismatch:
```
[POLY] ⚠ book drift: computed best_ask=47¢ but API says best_ask=48¢ for {ticker}
```
This catches missed messages, deserialization bugs, or API changes. Should be zero noise in normal operation.

**BUY side handling:**

12. For BUY side updates: do not apply to `PolyBook` (we don't track bids). But if `best_ask` is present, we can still use it for the sanity check above. This is free validation.

## Step 5: Handle edge cases

### 5a. Empty book after level removal

When the last level is removed, `best_yes_ask()` returns `None`. We must write `(0, 0)` to the `AtomicOrderbook` — price 0 means "no price available" and arb detection correctly skips markets with price 0 (`types.rs:377`).

### 5b. WebSocket reconnection

On reconnect, the bot receives fresh `book` snapshots for all subscribed tokens. `set_yes_asks()` / `set_no_asks()` clear the book before populating, so stale state from the previous connection is wiped.

No explicit `clear()` call is needed at reconnect time — the snapshot handler already does a full replacement.

### 5c. Multiple WebSocket connections

`polymarket.rs:305` splits tokens across multiple connections (max 500 per connection). Each connection processes its own subset of tokens. Since `PolyBook` is per-market (not per-connection) and protected by a `Mutex`, concurrent updates from different connections to different sides of the same market are safe. In practice this doesn't happen — a token only appears on one connection.

### 5d. Size semantics

Polymarket's `size` field is in **dollars** (e.g., `"200"` = $200). Our `parse_size()` converts to cents: `200 * 100 = 20000`. This fits in `u16` (max 65535 = $655.35). If a level has >$655 of depth, it saturates at `u16::MAX`. This matches the existing behavior and the `SizeCents` type constraint.

The `size` in `price_change` messages represents the **new total size at that price level** (absolute replacement), NOT a delta. If the level had 300 and a `price_change` says `size: "200"`, the new size is 200 — not 500.

### 5e. Size = 0 means level removal

Per the Polymarket API and the [reference Go client](https://pkg.go.dev/github.com/ivanzzeth/polymarket-go-real-time-data-client): when `size` is `"0"`, the level has been fully removed from the book. `update_yes_level(price, 0)` removes the entry from the BTreeMap.

## Step 6: Unit tests for `PolyBook` in `types.rs`

Add to the existing `#[cfg(test)] mod tests` block:

| Test | Proves |
|------|--------|
| `test_poly_book_new_is_empty` | Fresh book has no levels, `best_*_ask()` returns None |
| `test_poly_book_set_and_best_ask` | Snapshot populates correctly, best ask is lowest price |
| `test_poly_book_update_level_insert` | Insert new level via `update_yes_level` |
| `test_poly_book_update_level_replace` | Update existing level size |
| `test_poly_book_update_level_remove` | Size 0 removes the level |
| `test_poly_book_remove_best_reveals_next` | Remove best ask → next-best is promoted |
| `test_poly_book_remove_last_level` | Remove only level → None |
| `test_poly_book_snapshot_replaces_all` | Second `set_yes_asks` clears old levels |
| `test_poly_book_skips_zero_price_and_size` | Zero price or zero size levels are not inserted |
| `test_poly_book_clear` | `clear()` empties both sides |

## Step 7: Integration tests in `integration_tests.rs`

| Test | Proves |
|------|--------|
| `test_poly_snapshot_populates_book_and_cache` | Book snapshot → PolyBook populated → AtomicOrderbook correct |
| `test_poly_price_change_updates_book` | SELL price_change with size → PolyBook updated → AtomicOrderbook correct |
| `test_poly_best_ask_removed_reveals_next` | Remove best ask via size=0 → next-best promoted → no phantom arb |
| `test_poly_no_phantom_arb_after_removal` | End-to-end: arb exists → best ask removed → arb detection returns None |
| `test_poly_size_updates_from_price_change` | Size from price_change is used (not stale snapshot size) |
| `test_poly_empty_book_clears_cache` | All levels removed → cache shows (0, 0) → arb detection skips |
| `test_poly_snapshot_resets_after_deltas` | Deltas applied → snapshot arrives → book fully replaced |

These tests exercise `PolyBook` directly (since `process_book` and `process_price_change` are private). The pattern mirrors the `kalshi_delta_correctness` tests from #56.

## Verification

```bash
# 1. Build
cargo build --release

# 2. Run all tests
cargo test

# 3. Run PolyBook unit tests
cargo test poly_book

# 4. Run integration tests
cargo test poly_snapshot
cargo test poly_price_change
cargo test poly_phantom

# 5. Check for compiler warnings
cargo build --release 2>&1 | grep warning
```

## What This Does NOT Change

- **`AtomicOrderbook` layout** — Unchanged. The packed u64 format is the same.
- **`ArbOpportunity::detect()`** — Unchanged. It reads from `AtomicOrderbook` which is now fed by `PolyBook`.
- **Execution flow** — Unchanged. Execution reads `ArbOpportunity` fields, not the book directly.
- **Confirm queue validation** — Unchanged. It reads `market.poly.load()` which now reflects the true best ask.
- **WebSocket subscription/reconnection** — Unchanged. Only the message processing functions are modified.
- **KalshiBook** — Separate change in #56. The two shadow books are independent (different platforms, different delta semantics, different structs on `AtomicMarketState`).
