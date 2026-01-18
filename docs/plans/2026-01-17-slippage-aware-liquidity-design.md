# Slippage-Aware Liquidity Tracking

**Status:** Idea / Future Enhancement
**Date:** 2026-01-17
**Author:** Claude + Tyler

## Problem

Currently we only track the **best ask** (top-of-book) for each market:

```
Polymarket YES orderbook:
├─ 34¢ × 50¢ size    ← We store ONLY this
├─ 35¢ × 200¢ size   ← Ignored
├─ 38¢ × 500¢ size   ← Ignored
└─ 42¢ × 1000¢ size  ← Ignored
```

This causes issues:
1. **Rejected opportunities** - If best ask has <100¢ size, we reject even if deeper levels are profitable
2. **Thin markets** - Esports markets often have small top-of-book size but decent depth
3. **Conservative underestimation** - We miss arbs that would be profitable at slightly worse prices

## Proposed Solution

**Slippage-aware aggregation** - Walk the orderbook and sum sizes only for levels where the arb remains profitable.

### Algorithm

```rust
fn calculate_usable_liquidity(
    our_asks: &[PriceLevel],      // Side we're buying
    other_side_price: u16,         // Best price on other side
    fee_calculator: impl Fn(u16) -> u16,
) -> u16 {
    let mut usable_size = 0;

    for level in our_asks.iter().sorted_by_key(|l| l.price) {
        let total_cost = level.price + other_side_price + fee_calculator(level.price);

        if total_cost < 100 {  // Still profitable
            usable_size += level.size;
        } else {
            break;  // Deeper levels will be worse
        }
    }

    usable_size
}
```

### Example

Arb: Buy Poly YES + Buy Kalshi NO (Kalshi NO fixed at 62¢, fee ≈ 2¢)

| Poly Level | Price | Size | Total Cost | Profit | Include? |
|------------|-------|------|------------|--------|----------|
| 0 | 34¢ | 50¢ | 98¢ | +2¢ | ✅ |
| 1 | 35¢ | 200¢ | 99¢ | +1¢ | ✅ |
| 2 | 38¢ | 500¢ | 102¢ | -2¢ | ❌ |
| 3 | 42¢ | 1000¢ | 106¢ | -6¢ | ❌ |

**Result:** usable_size = 50 + 200 = 250¢ → 2 contracts

**Current behavior:** usable_size = 50¢ → 0 contracts (rejected)

## High-Level Implementation Plan

### Phase 1: Data Structure Changes

1. **Expand `AtomicOrderbook`** to store multiple levels
   - Option A: Store top 3-5 levels (fixed array)
   - Option B: Store aggregated "profitable depth" as single value

2. **Update packed format** or switch to separate atomics
   - Current: `[yes_ask:16][no_ask:16][yes_size:16][no_size:16]` = 64 bits
   - New might need: price + size for N levels, or separate fields

### Phase 2: WebSocket Handler Changes

1. **Polymarket `process_book`**: Extract top N ask levels instead of just best
2. **Kalshi handler**: Same treatment
3. **Calculate usable depth** at update time or query time

### Phase 3: Execution Changes

1. **Update `max_contracts` calculation** to use usable depth
2. **Tiered execution** (optional): Execute in tranches at each price level
3. **Profit tracking**: Track actual fill price vs expected

### Phase 4: Monitoring

1. **Log depth utilization**: How often do we use levels beyond top-of-book?
2. **Slippage tracking**: Actual fill price vs best ask
3. **Opportunity metrics**: How many more arbs captured?

## Considerations

### Pros
- Captures more profitable opportunities
- Better for thin markets (esports)
- More accurate liquidity assessment

### Cons
- More complex data structures
- Higher memory usage (N levels vs 1)
- Risk of overestimating if other side moves
- Execution complexity if doing tiered fills

### Open Questions

1. How many levels to track? (3? 5? 10?)
2. Recompute usable depth on every update or lazily at execution time?
3. Should execution be tiered (multiple orders at different prices) or single order?
4. How to handle the case where other side's price moves between detection and execution?

## Alternative Approaches Considered

1. **Keep current (top-of-book only)** - Simple, conservative, but misses opportunities
2. **Aggregate all depth naively** - Would overestimate and lead to bad fills
3. **Slippage-aware (this proposal)** - Best balance of accuracy and opportunity capture

## Files to Modify

- `controller/src/types.rs` - `AtomicOrderbook` struct
- `controller/src/polymarket.rs` - `process_book` function
- `controller/src/kalshi.rs` - Orderbook processing
- `controller/src/execution.rs` - `max_contracts` calculation
