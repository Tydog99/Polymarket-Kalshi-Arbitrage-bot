# PolyBook Sub-Cent Precision Fix

**Date:** 2026-02-12
**Branch:** `fix/polybook-subcent-precision`
**Issue:** Book drift warnings — `PolyBook` loses depth when multiple Polymarket price levels collide at the same whole-cent value.

## Problem

Polymarket's orderbook uses sub-cent prices (e.g., `0.999`, `0.998`, `0.99`). Our `parse_price` rounds these to whole cents (`PriceCents = u16`, range 0–99), so all three map to 99¢. `PolyBook` stores levels in a `BTreeMap<u16, u16>` keyed by cents, so `insert(99, size)` overwrites — only the last level's size survives.

When a delta later removes the 97¢ level (`size=0`), our book thinks that price is gone, but the API still sees sub-cent levels at ~0.97x that we lost during the snapshot. This produces persistent drift warnings and stale prices in the `AtomicOrderbook` cache.

**Evidence from live capture:**
```
Raw API snapshot for BRE-ARS NO token:
  ("0.999", "100000"), ("0.998", "55000"), ("0.99", "92075"), ("0.988", "5000"), ("0.98", "11815")

After parse_price → BTreeMap:
  99 → 92075  (lost 100000 + 55000 from 0.999 and 0.998)
  98 → 11815  (lost 5000 from 0.988)
```

## Solution: Milli-Cent Keys in PolyBook

Change `PolyBook` internal storage from `BTreeMap<u16, u16>` to `BTreeMap<u32, u32>` where:
- **Keys** = price × 1000 (milli-cents). `0.999` → 999, `0.998` → 998, `0.99` → 990. All unique.
- **Values** = size in milli-dollars (size × 1000). Avoids u16 overflow for large Polymarket sizes (API sends sizes like `260323.77`).

### New Types

```rust
/// Price in milli-cents (1 unit = $0.001). Range: 1–999.
pub type PolyPriceMillis = u32;

/// Size in milli-dollars (1 unit = $0.001). Allows large Polymarket sizes.
pub type PolySizeMillis = u32;
```

### New Parse Function

```rust
/// Parse Polymarket price string to milli-cents.
/// "0.999" → 999, "0.05" → 50, "0.5" → 500
pub fn parse_price_millis(s: &str) -> PolyPriceMillis;

/// Parse Polymarket size string to milli-dollars.
/// "260323.77" → 260323770
pub fn parse_size_millis(s: &str) -> PolySizeMillis;
```

### PolyBook Changes

```rust
pub struct PolyBook {
    yes_asks: BTreeMap<PolyPriceMillis, PolySizeMillis>,
    no_asks: BTreeMap<PolyPriceMillis, PolySizeMillis>,
}
```

Methods change signatures to use `PolyPriceMillis`/`PolySizeMillis` internally. The key method `best_yes_ask()` returns `Option<(PolyPriceMillis, PolySizeMillis)>`.

### Boundary Conversion

At the `PolyBook → AtomicOrderbook` boundary (in `process_book` and `process_price_change`), convert:

```rust
let (best_millis, size_millis) = pbook.best_yes_ask().unwrap_or((0, 0));
let price_cents = millis_to_cents(best_millis);      // 999 → 99, 50 → 5
let size_cents = (size_millis / 10) as SizeCents;     // milli-dollars → cents, clamped to u16
market.poly.update_yes(price_cents, size_cents);
```

`millis_to_cents` rounds down (ceiling would overstate the ask price), clamped to 0–99.

### What Does NOT Change

- `PriceCents`, `SizeCents` types (stay `u16`)
- `AtomicOrderbook` (stays packed u64 with cent precision)
- `ArbOpportunity::detect` (stays whole-cent arithmetic)
- `parse_price` (still used for Kalshi and for the `best_ask` sanity check field)
- Execution logic, position tracking, circuit breaker

### Drift Check Update

The BUY-side and SELL-side drift checks compare `our_best` (from `AtomicOrderbook`, in cents) against `api_best_ask` (parsed from the `best_ask` field, also in cents). Since both sides lose sub-cent info equally, the comparison stays valid. However, the drift should now be reduced significantly since the PolyBook tracks depth correctly.

## Testing

1. **Unit tests for `parse_price_millis`**: sub-cent strings, edge cases, 1-3 decimal places
2. **Unit tests for `parse_size_millis`**: large sizes, fractional sizes
3. **Unit tests for `millis_to_cents`**: boundary values, rounding
4. **PolyBook unit tests**: update existing tests to use millis, add tests with sub-cent levels that previously collided
5. **Integration test**: snapshot with duplicate-at-cent prices → verify no depth loss
6. **Integration test**: delta removes one sub-cent level → verify other sub-cent levels survive
