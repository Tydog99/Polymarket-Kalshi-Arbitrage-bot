# Arb Type Consolidation: Merge ArbOpportunity into FastExecutionRequest

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Date:** 2026-01-22
**Status:** Draft
**Goal:** Eliminate type duplication by consolidating `ArbOpportunity` and `FastExecutionRequest` into a single type, then rename to `ArbOpportunity`.

**Tech Stack:** Rust, no new dependencies

---

## Background

### Current State

Two types exist for representing arbitrage opportunities:

| Type | Location | Purpose |
|------|----------|---------|
| `ArbOpportunity` | `arb.rs` | Detection/validation phase |
| `FastExecutionRequest` | `types.rs` | Execution phase |

### The Problem

These types are nearly identical:

```rust
// ArbOpportunity (arb.rs)
struct ArbOpportunity {
    market_id: u16,
    arb_type: Option<ArbType>,  // None = invalid
    yes_price: u16,
    no_price: u16,
    yes_size: u16,
    no_size: u16,
    detected_ns: u64,
}

// FastExecutionRequest (types.rs)
struct FastExecutionRequest {
    market_id: u16,
    arb_type: ArbType,          // Always present
    yes_price: u16,
    no_price: u16,
    yes_size: u16,
    no_size: u16,
    detected_ns: u64,
    is_test: bool,              // Only difference
}
```

**Duplicated calculations across the codebase:**

| Calculation | Locations |
|-------------|-----------|
| `max_contracts()` | `ArbOpportunity::max_contracts()`, `PendingArb::max_contracts()`, `ExecutionEngine::process()` |
| `profit_cents()` | `ArbOpportunity::gross_profit_cents()`, `FastExecutionRequest::profit_cents()` |
| fee calculation | `arb::kalshi_fee()`, `confirm_queue.rs::validate_arb_detailed()` |

### Design Flaw

The two-type design was intended to provide type safety:
- `ArbOpportunity` with `Option<ArbType>` represents "maybe an arb"
- `FastExecutionRequest` with `ArbType` represents "definitely an arb"

But this doesn't provide meaningful safety because:
1. Invalid arbs (`arb_type = None`) are never stored or passed
2. `FastExecutionRequest::from_arb()` panics if called on invalid arbs
3. The `Option<FastExecutionRequest>` return from a factory provides the same semantic

### Target State

A single type with a factory function that returns `Option<Self>`:

```rust
impl ArbOpportunity {
    /// Detect arb from orderbook data. Returns None if no valid arb.
    pub fn detect(
        market_id: u16,
        kalshi: (u16, u16, u16, u16),
        poly: (u16, u16, u16, u16),
        config: &ArbConfig,
        detected_ns: u64,
    ) -> Option<Self> { ... }
}
```

---

## Task 1: Add `detect()` factory to FastExecutionRequest

**Files:**
- Modify: `controller/src/types.rs`

**Step 1: Write the failing test**

Add to `controller/src/types.rs` tests:

```rust
#[cfg(test)]
mod arb_detection_tests {
    use super::*;
    use crate::arb::ArbConfig;

    #[test]
    fn test_detect_returns_none_when_prices_too_high() {
        let config = ArbConfig::default();

        // Prices sum to 100 cents = no profit
        let result = FastExecutionRequest::detect(
            1,                          // market_id
            (50, 50, 1000, 1000),       // kalshi
            (50, 50, 1000, 1000),       // poly
            &config,
            12345,
        );

        assert!(result.is_none());
    }

    #[test]
    fn test_detect_finds_poly_yes_kalshi_no() {
        let config = ArbConfig::default(); // threshold = 99

        // Poly YES @ 45 + Kalshi NO @ 52 = 97 cents (+ ~1c fee) = ~98 < 99
        let result = FastExecutionRequest::detect(
            1,
            (55, 52, 500, 500),        // kalshi
            (45, 58, 500, 500),        // poly
            &config,
            12345,
        );

        let arb = result.expect("should detect arb");
        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo);
        assert_eq!(arb.yes_price, 45);  // poly yes
        assert_eq!(arb.no_price, 52);   // kalshi no
    }

    #[test]
    fn test_detect_returns_none_when_size_insufficient() {
        let config = ArbConfig::default();

        // Great prices but no size on kalshi NO side
        let result = FastExecutionRequest::detect(
            1,
            (55, 52, 500, 0),          // kalshi: no_size = 0
            (45, 58, 500, 500),
            &config,
            12345,
        );

        assert!(result.is_none());
    }
}
```

**Step 2: Implement `detect()` by moving logic from ArbOpportunity::new()**

```rust
impl FastExecutionRequest {
    /// Detect arbitrage opportunity from orderbook data.
    /// Returns Some if a valid arb exists, None otherwise.
    ///
    /// This is the single source of truth for arb detection logic.
    pub fn detect(
        market_id: u16,
        kalshi: (PriceCents, PriceCents, SizeCents, SizeCents),
        poly: (PriceCents, PriceCents, SizeCents, SizeCents),
        config: &crate::arb::ArbConfig,
        detected_ns: u64,
    ) -> Option<Self> {
        use crate::arb::kalshi_fee;

        let (k_yes, k_no, k_yes_size, k_no_size) = kalshi;
        let (p_yes, p_no, p_yes_size, p_no_size) = poly;

        // Check for invalid prices (0 = no price available)
        if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
            return None;
        }

        // Calculate Kalshi fees
        let k_yes_fee = kalshi_fee(k_yes);
        let k_no_fee = kalshi_fee(k_no);

        // Arb candidates in priority order
        let candidates = [
            (ArbType::PolyYesKalshiNo, p_yes + k_no + k_no_fee, p_yes, k_no, p_yes_size, k_no_size),
            (ArbType::KalshiYesPolyNo, k_yes + k_yes_fee + p_no, k_yes, p_no, k_yes_size, p_no_size),
            (ArbType::PolyOnly, p_yes + p_no, p_yes, p_no, p_yes_size, p_no_size),
            (ArbType::KalshiOnly, k_yes + k_yes_fee + k_no + k_no_fee, k_yes, k_no, k_yes_size, k_no_size),
        ];

        // Find first valid arb (lowest cost that beats threshold)
        for (arb_type, cost, yes_price, no_price, yes_size, no_size) in candidates {
            if cost <= config.threshold_cents {
                let max_contracts = (yes_size.min(no_size) as f64) / 100.0;

                if max_contracts >= config.min_contracts {
                    return Some(Self {
                        market_id,
                        arb_type,
                        yes_price,
                        no_price,
                        yes_size,
                        no_size,
                        detected_ns,
                        is_test: false,
                    });
                }
            }
        }

        None
    }
}
```

**Step 3: Run tests**

```bash
cargo test -p controller arb_detection_tests
```

**Step 4: Commit**

```bash
git add controller/src/types.rs
git commit -m "$(cat <<'EOF'
feat(types): add detect() factory to FastExecutionRequest

Move arb detection logic into FastExecutionRequest::detect().
Returns Option<Self> - None if no valid arb, Some if valid.
This is preparation for eliminating the duplicate ArbOpportunity type.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add missing methods to FastExecutionRequest

**Files:**
- Modify: `controller/src/types.rs`

Ensure `FastExecutionRequest` has all methods currently on `ArbOpportunity`:

**Step 1: Add max_contracts() if not present**

```rust
impl FastExecutionRequest {
    /// Calculate max executable contracts (minimum of both sides)
    #[inline]
    pub fn max_contracts(&self) -> u16 {
        if self.yes_price == 0 || self.no_price == 0 {
            return 0;
        }
        let yes_contracts = self.yes_size / self.yes_price;
        let no_contracts = self.no_size / self.no_price;
        yes_contracts.min(no_contracts)
    }
}
```

**Step 2: Verify profit_cents() exists and matches**

The method should match `ArbOpportunity::gross_profit_cents()` logic.

**Step 3: Commit**

```bash
git add controller/src/types.rs
git commit -m "$(cat <<'EOF'
feat(types): add max_contracts() to FastExecutionRequest

Consolidate contract calculation into single location.
Previously duplicated in ArbOpportunity, PendingArb, and ExecutionEngine.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Migrate callers from ArbOpportunity::new() to FastExecutionRequest::detect()

**Files:**
- Modify: `controller/src/kalshi.rs`
- Modify: `controller/src/polymarket.rs`
- Modify: `controller/src/main.rs`
- Modify: `controller/src/confirm_queue.rs`

**Step 1: Update kalshi.rs**

Before:
```rust
let arb = ArbOpportunity::new(market_id, kalshi, poly, config, ts);
if arb.is_valid() {
    let request = FastExecutionRequest::from_arb(&arb);
    // send request
}
```

After:
```rust
if let Some(request) = FastExecutionRequest::detect(market_id, kalshi, poly, config, ts) {
    // send request directly
}
```

**Step 2: Update polymarket.rs**

Same pattern as kalshi.rs.

**Step 3: Update main.rs (startup sweep, heartbeat)**

Same pattern.

**Step 4: Update confirm_queue.rs validate_arb()**

Before:
```rust
pub fn validate_arb(&self, arb: &PendingArb, config: &ArbConfig) -> Option<ArbOpportunity> {
    let opp = ArbOpportunity::new(...);
    if opp.is_valid() { Some(opp) } else { None }
}
```

After:
```rust
pub fn validate_arb(&self, arb: &PendingArb, config: &ArbConfig) -> Option<FastExecutionRequest> {
    FastExecutionRequest::detect(...)
}
```

**Step 5: Run tests**

```bash
cargo test -p controller
cargo check -p controller
```

**Step 6: Commit**

```bash
git add controller/src/kalshi.rs controller/src/polymarket.rs controller/src/main.rs controller/src/confirm_queue.rs
git commit -m "$(cat <<'EOF'
refactor: migrate from ArbOpportunity::new() to FastExecutionRequest::detect()

All arb detection now uses FastExecutionRequest::detect() which returns
Option<Self>. This eliminates the intermediate ArbOpportunity type and
removes the from_arb() conversion step.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Update PendingArb to delegate to FastExecutionRequest

**Files:**
- Modify: `controller/src/confirm_queue.rs`

**Step 1: Remove duplicate max_contracts()**

Before:
```rust
impl PendingArb {
    pub fn max_contracts(&self) -> i64 {
        if self.request.yes_price == 0 || self.request.no_price == 0 {
            return 0;
        }
        let yes_contracts = self.request.yes_size / self.request.yes_price;
        let no_contracts = self.request.no_size / self.request.no_price;
        yes_contracts.min(no_contracts) as i64
    }
}
```

After:
```rust
impl PendingArb {
    pub fn max_contracts(&self) -> u16 {
        self.request.max_contracts()
    }
}
```

**Step 2: Verify profit_cents() delegates**

```rust
impl PendingArb {
    pub fn profit_cents(&self) -> i16 {
        self.request.profit_cents()
    }
}
```

**Step 3: Commit**

```bash
git add controller/src/confirm_queue.rs
git commit -m "$(cat <<'EOF'
refactor(confirm): delegate calculations to FastExecutionRequest

PendingArb.max_contracts() and profit_cents() now delegate to
FastExecutionRequest methods, eliminating duplicate calculation logic.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Remove from_arb() from FastExecutionRequest

**Files:**
- Modify: `controller/src/types.rs`

**Step 1: Verify no remaining callers**

```bash
grep -r "from_arb" controller/src/
```

Expected: Only the definition, no callers.

**Step 2: Delete the method**

Remove:
```rust
pub fn from_arb(arb: &crate::arb::ArbOpportunity) -> Self { ... }
```

**Step 3: Commit**

```bash
git add controller/src/types.rs
git commit -m "$(cat <<'EOF'
refactor(types): remove from_arb() conversion method

No longer needed since callers use detect() directly.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Delete ArbOpportunity struct

**Files:**
- Modify: `controller/src/arb.rs`

**Step 1: Verify no remaining references**

```bash
grep -r "ArbOpportunity" controller/src/
```

Expected: Only the definition in arb.rs and possibly tests.

**Step 2: Delete the struct and its impl block**

Keep in `arb.rs`:
- `ArbConfig` struct and impl
- `kalshi_fee()` function
- Tests for `ArbConfig` and `kalshi_fee()`

Delete:
- `ArbOpportunity` struct
- All `ArbOpportunity` methods
- `ArbOpportunity` tests

**Step 3: Update arb.rs module doc**

```rust
//! Centralized arbitrage configuration and fee calculation.
//!
//! Arb detection logic lives in `FastExecutionRequest::detect()`.
//! This module provides:
//! - `ArbConfig` - threshold and min_contracts configuration
//! - `kalshi_fee()` - Kalshi fee calculation
```

**Step 4: Run tests**

```bash
cargo test -p controller
cargo check -p controller
```

**Step 5: Commit**

```bash
git add controller/src/arb.rs
git commit -m "$(cat <<'EOF'
refactor(arb): delete ArbOpportunity struct

Detection logic now lives in FastExecutionRequest::detect().
arb.rs retains ArbConfig and kalshi_fee() for configuration and fees.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Rename FastExecutionRequest to ArbOpportunity

**Files:**
- Modify: `controller/src/types.rs`
- Modify: All files that reference `FastExecutionRequest`

**Step 1: Rename the struct**

In `types.rs`:
```rust
// Before
pub struct FastExecutionRequest { ... }

// After
pub struct ArbOpportunity { ... }
```

**Step 2: Add type alias for gradual migration (optional)**

```rust
/// Deprecated: Use ArbOpportunity instead
#[deprecated(note = "Renamed to ArbOpportunity")]
pub type FastExecutionRequest = ArbOpportunity;
```

**Step 3: Update all references**

Files to update:
- `controller/src/types.rs` - definition and impls
- `controller/src/kalshi.rs` - channel types, function signatures
- `controller/src/polymarket.rs` - channel types, function signatures
- `controller/src/main.rs` - channel creation, function calls
- `controller/src/execution.rs` - process() signature
- `controller/src/confirm_queue.rs` - PendingArb.request type
- `controller/src/remote_execution.rs` - serialization
- Test files

**Step 4: Run tests**

```bash
cargo test -p controller
cargo check -p controller
```

**Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor: rename FastExecutionRequest to ArbOpportunity

Single type now represents a validated arbitrage opportunity.
- detect() factory returns Option<ArbOpportunity>
- Contains all arb data: market_id, prices, sizes, arb_type
- Provides max_contracts() and profit_cents() calculations

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Remove deprecated type alias

**Files:**
- Modify: `controller/src/types.rs`

**Step 1: Verify no remaining FastExecutionRequest references**

```bash
grep -r "FastExecutionRequest" controller/
```

**Step 2: Remove the type alias**

Delete:
```rust
#[deprecated(note = "Renamed to ArbOpportunity")]
pub type FastExecutionRequest = ArbOpportunity;
```

**Step 3: Commit**

```bash
git add controller/src/types.rs
git commit -m "$(cat <<'EOF'
chore: remove deprecated FastExecutionRequest type alias

All code now uses ArbOpportunity directly.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Update documentation

**Files:**
- Modify: `CLAUDE.md`
- Delete: `docs/plans/2026-01-21-arb-opportunity-refactor.md` (superseded)

**Step 1: Update CLAUDE.md architecture section**

Update the data flow diagram:
```
WebSocket Price Updates (kalshi.rs, polymarket.rs)
    ↓
Global State with Lock-Free Orderbook Cache (types.rs)
    ↓
ArbOpportunity::detect() in WebSocket Handlers
    ↓
Confirmation Queue (confirm_queue.rs) or Direct Execution
    ↓
...
```

**Step 2: Commit**

```bash
git add CLAUDE.md
git rm docs/plans/2026-01-21-arb-opportunity-refactor.md
git commit -m "$(cat <<'EOF'
docs: update architecture for ArbOpportunity consolidation

- Update CLAUDE.md data flow to show ArbOpportunity::detect()
- Remove superseded refactor plan

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Summary

After completing all tasks:

| Before | After |
|--------|-------|
| `ArbOpportunity` + `FastExecutionRequest` | Single `ArbOpportunity` type |
| `ArbOpportunity::new()` + `from_arb()` | `ArbOpportunity::detect() -> Option<Self>` |
| `max_contracts()` in 4 places | Single method on `ArbOpportunity` |
| `profit_cents()` in 3 places | Single method on `ArbOpportunity` |
| 2 structs with 7 duplicate fields | 1 struct, no duplication |

**Benefits:**
1. Single source of truth for arb representation
2. No duplicate calculations
3. Cleaner API: `detect()` returns `Option<Self>` instead of requiring validity check
4. Better name: `ArbOpportunity` is more descriptive than `FastExecutionRequest`
