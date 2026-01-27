# Orderbook Depth: Multi-Level Price Cache

## Overview

`OrderbookDepth` (`types.rs`) stores 3 price levels per side for each platform, enabling EV-maximizing arbitrage detection. It replaces the previous `AtomicOrderbook` which only stored top-of-book.

## Why This Design?

1. **EV Maximization**: Find arbs with higher expected value by walking the book
2. **Volume-aware**: When best price has low volume, check if worse price has more
3. **Instrumented**: Lock contention monitoring for performance visibility
4. **A/B Testable**: `USE_DEPTH` env var toggles between old and new detection

## Data Structures

### PriceLevel

```rust
#[derive(Clone, Copy, Default, Debug)]
pub struct PriceLevel {
    pub price: PriceCents,  // 0-99 cents (0 = no price)
    pub size: SizeCents,    // liquidity in cents
}
```

### OrderbookDepth

```rust
pub const DEPTH_LEVELS: usize = 3;

#[derive(Clone, Copy, Default, Debug)]
pub struct OrderbookDepth {
    pub yes_levels: [PriceLevel; DEPTH_LEVELS],  // Sorted by price ascending
    pub no_levels: [PriceLevel; DEPTH_LEVELS],   // Sorted by price ascending
}

impl OrderbookDepth {
    /// Backward-compatible access to best prices
    pub fn top_of_book(&self) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
        (
            self.yes_levels[0].price,
            self.no_levels[0].price,
            self.yes_levels[0].size,
            self.no_levels[0].size,
        )
    }
}
```

### InstrumentedRwLock<T>

Wrapper around `std::sync::RwLock` with contention monitoring:

```rust
pub struct InstrumentedRwLock<T> {
    inner: RwLock<T>,
    contention_count: AtomicU64,
    total_writes: AtomicU64,
    total_reads: AtomicU64,
    total_wait_ns: AtomicU64,
}
```

**Fast path**: Uses `try_write()`/`try_read()` first (~10-15ns overhead)
**Slow path**: Only measures wait time when actual contention occurs

## Storage in AtomicMarketState

```rust
pub struct AtomicMarketState {
    pub kalshi: InstrumentedRwLock<OrderbookDepth>,
    pub poly: InstrumentedRwLock<OrderbookDepth>,
    // ... other fields
}
```

## Detection Modes

### Top-of-Book (Default)

```bash
dotenvx run -- cargo run --release
```

Uses `ArbOpportunity::detect()` - only examines `levels[0]` (best price).

### Depth-Aware (`USE_DEPTH=1`)

```bash
USE_DEPTH=1 dotenvx run -- cargo run --release
```

Uses `detect_best_ev()` - examines all 81 combinations (3^4):
- For each Kalshi YES level (0-2)
- For each Kalshi NO level (0-2)
- For each Poly YES level (0-2)
- For each Poly NO level (0-2)

Selects the combination with highest **EV = |gap| × contracts**.

### Example

```
Level 0: -2¢ gap × 3 contracts = 6¢ EV
Level 1: -1¢ gap × 50 contracts = 50¢ EV  ← SELECTED
Level 2: 0¢ gap × 200 contracts = 0¢ EV
```

## WebSocket Update Flow

### Kalshi (kalshi.rs)

1. Receive orderbook snapshot/delta
2. Collect top 3 NO bids, convert to YES asks: `ask = 100 - bid`
3. Collect top 3 YES bids, convert to NO asks
4. Store via `market.kalshi.write()`
5. Call `detect_arb()` for arb detection

### Polymarket (polymarket.rs)

1. Receive book snapshot
2. Collect top 3 asks for YES token, sorted by price ascending
3. Collect top 3 asks for NO token
4. Store via `market.poly.write()`
5. Call `detect_arb()` for arb detection

## Lock Contention Monitoring

### Global Aggregator

```rust
lazy_static! {
    pub static ref LOCK_STATS: LockStatsAggregator = LockStatsAggregator::new();
}
```

Every lock operation reports to `LOCK_STATS`:
- Contention count (when `try_*` fails)
- Operation count
- Wait time in nanoseconds

### Heartbeat Output

```
[LOCK] ops=45000 contention=12 (0.03%) avg_wait=850µs
```

**Healthy**: < 1% contention, < 100µs avg wait
**Concerning**: 1-5% contention, 100µs-1ms avg wait
**Critical**: > 10% contention, > 1ms avg wait

## Memory Layout

| Component | Size per market |
|-----------|-----------------|
| OrderbookDepth × 2 platforms | 96 bytes |
| InstrumentedRwLock overhead × 2 | 64 bytes |
| **Total** | **160 bytes** |

For 1000 markets: ~160 KB (vs ~16 KB with old AtomicOrderbook)

## Common Patterns

### Reading depth

```rust
let depth = market.kalshi.read();
let (yes_ask, no_ask, yes_size, no_size) = depth.top_of_book();
// Or access all levels:
for level in &depth.yes_levels {
    if level.price > 0 {
        // Valid level
    }
}
```

### Writing depth

```rust
let mut guard = market.kalshi.write();
*guard = OrderbookDepth {
    yes_levels: [level1, level2, level3],
    no_levels: [level1, level2, level3],
};
```

### Avoid holding guards across await

```rust
// BAD - guard held across await
if let Some(arb) = detect_arb(&*market.kalshi.read(), ...) {
    route_arb(...).await;  // ❌ RwLockReadGuard not Send
}

// GOOD - extract data before await
let kalshi_depth = market.kalshi.read().clone();
if let Some(arb) = detect_arb(&kalshi_depth, ...) {
    route_arb(...).await;  // ✅
}
```

## Migration from AtomicOrderbook

The old `AtomicOrderbook` is retained for reference but no longer used:

| Old API | New API |
|---------|---------|
| `market.kalshi.load()` | `market.kalshi.read().top_of_book()` |
| `market.kalshi.store(...)` | `*market.kalshi.write() = depth` |
| `market.kalshi.update_yes(...)` | Modify via write guard |

## See Also

- `arb.rs` - `detect_arb()`, `detect_best_ev()`, `use_depth_detection()`
- `types.rs` - `OrderbookDepth`, `PriceLevel`, `InstrumentedRwLock`, `LockStatsAggregator`
- `kalshi.rs` - `process_kalshi_snapshot()`, `process_kalshi_delta()`
- `polymarket.rs` - `process_book()`
