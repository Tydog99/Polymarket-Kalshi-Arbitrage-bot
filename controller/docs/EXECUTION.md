# Execution Engine Architecture

## Overview

The execution engine (`execution.rs`) handles concurrent order execution across Kalshi and Polymarket, with automatic exposure management for mismatched fills.

## Key Components

| Component | Purpose |
|-----------|---------|
| `ExecutionEngine` | Core engine struct with platform clients, circuit breaker, position channel |
| `NanoClock` | High-precision monotonic clock for latency measurement |
| `FastExecutionRequest` | Incoming arb opportunity with prices, sizes, timestamps |
| `ExecutionResult` | Outcome with success flag, profit, latency, error |

## In-Flight Deduplication

Prevents duplicate execution of the same market opportunity using a lock-free bitmask.

### Data Structure

```rust
in_flight: Arc<[AtomicU64; 8]>  // 8 slots × 64 bits = 512 markets
```

### Bit Addressing

```
market_id = 137
slot = 137 / 64 = 2
bit  = 137 % 64 = 9

mask = 1u64 << 9  // 0x0000_0000_0000_0200
```

### Operations

**Set (try to acquire):**
```rust
let prev = self.in_flight[slot].fetch_or(mask, Ordering::AcqRel);
if prev & mask != 0 {
    // Already in-flight, reject
    return Err("Already in-flight");
}
```

**Release (immediate):**
```rust
let mask = !(1u64 << bit);  // Inverted mask
self.in_flight[slot].fetch_and(mask, Ordering::Release);
```

**Release (delayed 10s):**
```rust
tokio::spawn(async move {
    tokio::time::sleep(Duration::from_secs(10)).await;
    in_flight[slot].fetch_and(mask, Ordering::Release);
});
```

### Why Delayed Release?

Low-liquidity markets can spam opportunities. The 10-second delay prevents:
- Rapid re-detection of the same stale opportunity
- Wasted API calls when the arb has already been consumed

Used for: dry runs, insufficient liquidity, successful execution.

## Execution Flow

```
FastExecutionRequest received
        │
        ▼
┌─────────────────────────────────┐
│ 1. In-flight check (bitmask)    │ ← Reject if already processing
└─────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────┐
│ 2. Validate market exists       │
└─────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────┐
│ 3. Check profit threshold (≥1¢) │
└─────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────┐
│ 4. Check liquidity (≥1 contract)│
└─────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────┐
│ 5. Circuit breaker capacity     │ ← May cap contract count
└─────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────┐
│ 6. Execute both legs (async)    │ ← Concurrent platform calls
└─────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────┐
│ 7. Handle fill mismatch         │ ← Auto-close if needed
└─────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────┐
│ 8. Record fills, release slot   │
└─────────────────────────────────┘
```

## Concurrent Leg Execution

Both legs execute in parallel using `tokio::join!`:

```rust
// Simplified example for PolyYesKalshiNo
let (poly_result, kalshi_result) = tokio::join!(
    poly_client.buy_yes(token, price, contracts),
    kalshi_client.buy_no(ticker, price, contracts)
);
```

This minimizes latency but means fills can mismatch (one succeeds, other fails).

## Auto-Close on Mismatched Fills

When fills don't match, the system has unhedged exposure. Auto-close fixes this.

### Detection

```rust
if yes_filled != no_filled && (yes_filled > 0 || no_filled > 0) {
    let excess = (yes_filled - no_filled).abs();
    // Spawn auto-close in background
}
```

### Algorithm

1. **Settlement wait**: 2 seconds for Polymarket on-chain settlement
2. **Price walk**: Start at original price, step down 1¢ per retry
3. **Floor**: Never go below 1¢
4. **Retry loop**: Until fully closed or floor reached

```
Original fill: 5 YES @ 52¢, 0 NO (mismatch!)
                    │
                    ▼
        Wait 2s for Poly settlement
                    │
                    ▼
        Attempt SELL 5 YES @ 52¢
        Result: 3 filled
                    │
                    ▼
        Step down to 51¢
        Attempt SELL 2 YES @ 51¢
        Result: 2 filled
                    │
                    ▼
        ✅ Closed 5 contracts
```

### Configuration

| Constant | Value | Description |
|----------|-------|-------------|
| `MIN_PRICE_CENTS` | 1 | Floor price |
| `PRICE_STEP_CENTS` | 1 | Decrement per retry |
| `RETRY_DELAY_MS` | 100 | Delay between retries |
| Settlement wait | 2000ms | Polymarket only |

## Position Recording

Successful fills are recorded via `PositionChannel`:

```rust
self.position_channel.record_fill(FillRecord::new(
    &pair.pair_id,
    &pair.description,
    platform,  // "kalshi" or "polymarket"
    side,      // "yes" or "no"
    contracts,
    avg_price,
    fee,
    &order_id,
));
```

## Error Handling

| Error | Handling |
|-------|----------|
| Already in-flight | Immediate reject, no release (already held) |
| Profit < 1¢ | Immediate release |
| Insufficient liquidity | Delayed release (10s) |
| Circuit breaker | Immediate release |
| Execution failure | Record error, immediate release |
| Fill mismatch | Background auto-close task |

## Debugging

**Check in-flight state:**
```rust
// In tests or debug code
let slot = (market_id / 64) as usize;
let bit = market_id % 64;
let state = engine.in_flight[slot].load(Ordering::Acquire);
let is_in_flight = (state >> bit) & 1 == 1;
```

**Log prefixes:**
- `[EXEC] 📡` - Arb detected
- `[EXEC] 🏃` - Dry run
- `[EXEC] ✅` - Success
- `[EXEC] ⚠️` - Warning (mismatch, capped)
- `[EXEC] ❌` - Error
- `[EXEC] 🔄` - Auto-close in progress
