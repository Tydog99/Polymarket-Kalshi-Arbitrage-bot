# AtomicOrderbook: Lock-Free Price Cache

> **⚠️ DEPRECATED**: This design has been replaced by `OrderbookDepth` with `InstrumentedRwLock`.
> See `controller/docs/ORDERBOOK_DEPTH.md` for the current implementation.
>
> The `AtomicOrderbook` code is retained for reference but is no longer used in production.
> The new design stores 3 price levels per side (vs 1) for EV-maximizing arbitrage detection.

## Overview (Historical)

`AtomicOrderbook` (`types.rs`) provides lock-free, thread-safe orderbook state for a single trading platform. It packs four 16-bit values into a single `AtomicU64` for cache-line efficiency.

## Why This Design?

1. **Zero-copy updates**: WebSocket threads update prices without locks
2. **Cache-line aligned**: `#[repr(align(64))]` prevents false sharing
3. **Single atomic load**: Read all four values atomically in one operation
4. **SIMD-friendly**: Packed u64 enables vectorized arbitrage scanning

## Bit Layout

```
┌────────────────────────────────────────────────────────────────┐
│                         u64 (64 bits)                          │
├────────────┬────────────┬────────────┬────────────────────────┤
│  yes_ask   │   no_ask   │  yes_size  │       no_size          │
│  bits 48-63│  bits 32-47│  bits 16-31│       bits 0-15        │
│   16 bits  │   16 bits  │   16 bits  │        16 bits         │
└────────────┴────────────┴────────────┴────────────────────────┘
```

### Value Ranges

| Field | Type | Range | Meaning |
|-------|------|-------|---------|
| `yes_ask` | `PriceCents` | 0-99 | YES ask price in cents (0 = no price) |
| `no_ask` | `PriceCents` | 0-99 | NO ask price in cents (0 = no price) |
| `yes_size` | `SizeCents` | 0-65535 | YES liquidity in cents (~$655 max) |
| `no_size` | `SizeCents` | 0-65535 | NO liquidity in cents (~$655 max) |

## Pack/Unpack Functions

```rust
/// Pack four values into u64
pub fn pack_orderbook(yes_ask: u16, no_ask: u16, yes_size: u16, no_size: u16) -> u64 {
    ((yes_ask as u64) << 48) |
    ((no_ask as u64) << 32) |
    ((yes_size as u64) << 16) |
    (no_size as u64)
}

/// Unpack u64 into four values
pub fn unpack_orderbook(packed: u64) -> (u16, u16, u16, u16) {
    let yes_ask  = ((packed >> 48) & 0xFFFF) as u16;
    let no_ask   = ((packed >> 32) & 0xFFFF) as u16;
    let yes_size = ((packed >> 16) & 0xFFFF) as u16;
    let no_size  = (packed & 0xFFFF) as u16;
    (yes_ask, no_ask, yes_size, no_size)
}
```

## AtomicOrderbook Operations

### Full Load (read all values)

```rust
pub fn load(&self) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
    unpack_orderbook(self.packed.load(Ordering::Acquire))
}
```

### Full Store (write all values)

```rust
pub fn store(&self, yes_ask: PriceCents, no_ask: PriceCents,
             yes_size: SizeCents, no_size: SizeCents) {
    self.packed.store(
        pack_orderbook(yes_ask, no_ask, yes_size, no_size),
        Ordering::Release
    );
}
```

### Partial Update (YES side only)

Uses compare-and-swap loop to preserve NO side:

```rust
pub fn update_yes(&self, yes_ask: PriceCents, yes_size: SizeCents) {
    let mut current = self.packed.load(Ordering::Acquire);
    loop {
        let (_, no_ask, _, no_size) = unpack_orderbook(current);
        let new = pack_orderbook(yes_ask, no_ask, yes_size, no_size);
        match self.packed.compare_exchange_weak(
            current, new, Ordering::AcqRel, Ordering::Acquire
        ) {
            Ok(_) => break,
            Err(c) => current = c,  // Retry with updated value
        }
    }
}
```

### Partial Update (NO side only)

```rust
pub fn update_no(&self, no_ask: PriceCents, no_size: SizeCents) {
    // Same CAS loop pattern, preserving YES side
}
```

## Memory Ordering

| Operation | Ordering | Why |
|-----------|----------|-----|
| `load()` | `Acquire` | See all writes before this point |
| `store()` | `Release` | Publish writes to other threads |
| CAS success | `AcqRel` | Both acquire and release semantics |
| CAS failure | `Acquire` | Re-read current value |

## Usage in GlobalState

```rust
pub struct MarketState {
    pub kalshi: AtomicOrderbook,
    pub poly: AtomicOrderbook,
    pair: RwLock<Option<MarketPair>>,
}

// WebSocket update (Kalshi thread)
market.kalshi.update_yes(52, 1000);  // 52¢ ask, $10 liquidity

// Arbitrage detection (heartbeat thread)
let (k_yes, k_no, k_yes_sz, k_no_sz) = market.kalshi.load();
let (p_yes, p_no, p_yes_sz, p_no_sz) = market.poly.load();

if k_yes + p_no < 100 {
    // Arbitrage exists!
}
```

## Common Mistakes

### Wrong: Reading fields individually
```rust
// BAD - not atomic, can see inconsistent state
let yes = market.kalshi.yes_ask.load(...);  // ❌ Doesn't exist
let no = market.kalshi.no_ask.load(...);     // ❌
```

### Right: Load all at once
```rust
// GOOD - atomic read of all fields
let (yes, no, yes_sz, no_sz) = market.kalshi.load();  // ✅
```

### Wrong: Forgetting CAS loop can fail
```rust
// BAD - compare_exchange_weak can spuriously fail
if self.packed.compare_exchange_weak(...).is_err() {
    return;  // ❌ Silently drops update
}
```

### Right: Always loop on CAS
```rust
// GOOD - retry until success
loop {
    match self.packed.compare_exchange_weak(...) {
        Ok(_) => break,
        Err(c) => current = c,  // ✅ Retry
    }
}
```

## Performance Notes

1. **Cache-line isolation**: The `#[repr(align(64))]` ensures each `AtomicOrderbook` sits on its own cache line, preventing false sharing between markets

2. **No locks**: Updates are wait-free for full stores, lock-free for partial updates

3. **Contention**: If YES and NO sides update simultaneously, CAS retries occur. In practice, this is rare since updates come from different message types

## Sentinel Values

| Value | Constant | Meaning |
|-------|----------|---------|
| 0 | `NO_PRICE` | No price available (market closed/no liquidity) |

Always check for `NO_PRICE` before using prices:

```rust
let (yes_ask, no_ask, _, _) = market.kalshi.load();
if yes_ask == NO_PRICE || no_ask == NO_PRICE {
    // Skip - incomplete orderbook
    continue;
}
```
