# ArbOpportunity Refactor Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Centralize arbitrage detection logic into a single `ArbOpportunity` type that owns all arb business rules, eliminating duplicated code in kalshi.rs and polymarket.rs.

**Architecture:** Callers construct `ArbOpportunity::new()` with raw orderbook data + `ArbConfig`. The struct internally determines if a valid arb exists based on threshold, fees, and min_contracts. Callers check `is_valid()` and send to execution if true.

**Tech Stack:** Rust, no new dependencies

---

## Background

### Current Problems

1. **Arb detection is split across files:**
   - `types.rs::check_arbs()` - checks prices only, returns bitmask
   - `kalshi.rs::send_kalshi_arb_request()` - loads sizes, constructs request
   - `polymarket.rs::send_arb_request()` - duplicate of above
   - `confirm_queue.rs::push()` - another size check (`min_size < 100`)
   - `confirm_queue.rs::validate_arb_detailed()` - recalculates costs with fees
   - `execution.rs::process()` - validates contracts > 0

2. **Size 0 arbs reach execution:**
   - `check_arbs()` ignores sizes entirely
   - Sizes loaded separately after price check
   - Race condition possible between price check and size load
   - Execution rejects with "Insufficient liquidity" after the fact

3. **Duplicated logic:**
   - `send_arb_request()` and `send_kalshi_arb_request()` are nearly identical
   - Both manually decode arb_mask bitmask
   - Both construct `FastExecutionRequest` the same way

### Target State

- Single `ArbOpportunity` struct owns all arb logic
- `ArbConfig` built once at startup from env vars
- Callers pass raw data, struct determines validity
- No more bitmask decoding scattered across files
- `FastExecutionRequest` becomes a thin wrapper or is replaced

---

## Task 1: Create ArbConfig struct

**Files:**
- Create: `controller/src/arb.rs`
- Modify: `controller/src/lib.rs` (add module)

**Step 1: Write the failing test**

Add to `controller/src/arb.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arb_config_defaults() {
        // Clear any env vars that might interfere
        std::env::remove_var("ARB_THRESHOLD_CENTS");
        std::env::remove_var("ARB_MIN_CONTRACTS");

        let config = ArbConfig::from_env();

        assert_eq!(config.threshold_cents, 99); // 0.995 * 100 rounded
        assert!((config.min_contracts - 1.0).abs() < 0.001);
        assert!((config.kalshi_fee_rate - 0.07).abs() < 0.001);
        assert!((config.poly_fee_rate - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_arb_config_from_env() {
        std::env::set_var("ARB_THRESHOLD_CENTS", "95");
        std::env::set_var("ARB_MIN_CONTRACTS", "0.5");

        let config = ArbConfig::from_env();

        assert_eq!(config.threshold_cents, 95);
        assert!((config.min_contracts - 0.5).abs() < 0.001);

        // Cleanup
        std::env::remove_var("ARB_THRESHOLD_CENTS");
        std::env::remove_var("ARB_MIN_CONTRACTS");
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p controller arb::tests::test_arb_config --no-default-features`
Expected: FAIL - module `arb` not found

**Step 3: Write minimal implementation**

Create `controller/src/arb.rs`:

```rust
//! Centralized arbitrage opportunity detection.
//!
//! This module provides the canonical definition of what constitutes an arbitrage
//! opportunity, including price thresholds, fee calculations, and minimum size requirements.

use crate::types::{PriceCents, SizeCents};

/// Configuration for arbitrage detection, built from environment at startup.
#[derive(Debug, Clone)]
pub struct ArbConfig {
    /// Price threshold in cents - arb exists when total cost < this (default: 99 = $0.99)
    pub threshold_cents: PriceCents,

    /// Minimum contracts to consider an arb valid (default: 1.0)
    pub min_contracts: f64,

    /// Kalshi fee rate (default: 0.07 = 7%)
    pub kalshi_fee_rate: f64,

    /// Polymarket fee rate (default: 0.0 = no fees)
    pub poly_fee_rate: f64,
}

impl ArbConfig {
    /// Build configuration from environment variables.
    ///
    /// Environment variables:
    /// - `ARB_THRESHOLD_CENTS`: Price threshold in cents (default: 99)
    /// - `ARB_MIN_CONTRACTS`: Minimum contracts for valid arb (default: 1.0)
    pub fn from_env() -> Self {
        Self {
            threshold_cents: std::env::var("ARB_THRESHOLD_CENTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(99), // 0.995 * 100 rounded down

            min_contracts: std::env::var("ARB_MIN_CONTRACTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0),

            kalshi_fee_rate: 0.07,
            poly_fee_rate: 0.0,
        }
    }
}

impl Default for ArbConfig {
    fn default() -> Self {
        Self {
            threshold_cents: 99,
            min_contracts: 1.0,
            kalshi_fee_rate: 0.07,
            poly_fee_rate: 0.0,
        }
    }
}
```

Add to `controller/src/lib.rs`:

```rust
pub mod arb;
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p controller arb::tests::test_arb_config`
Expected: PASS

**Step 5: Commit**

```bash
git add controller/src/arb.rs controller/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(arb): add ArbConfig struct for centralized arb configuration

Introduces ArbConfig which loads arb detection parameters from
environment variables at startup. This is the foundation for
centralizing all arb logic into a single module.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Create ArbOpportunity struct with basic detection

**Files:**
- Modify: `controller/src/arb.rs`

**Step 1: Write the failing test**

Add to `controller/src/arb.rs` tests:

```rust
#[test]
fn test_arb_opportunity_no_arb_when_prices_too_high() {
    let config = ArbConfig::default();

    // Prices sum to 100 cents = no profit
    let arb = ArbOpportunity::new(
        1,                          // market_id
        (50, 50, 1000, 1000),       // kalshi: yes_ask=50, no_ask=50, sizes=1000
        (50, 50, 1000, 1000),       // poly: same
        &config,
        12345,                      // detected_ns
    );

    assert!(!arb.is_valid());
    assert!(arb.arb_type().is_none());
}

#[test]
fn test_arb_opportunity_detects_poly_yes_kalshi_no() {
    let config = ArbConfig::default(); // threshold = 99

    // Poly YES @ 45 + Kalshi NO @ 52 = 97 cents (+ ~1c fee) = ~98 < 99 threshold
    let arb = ArbOpportunity::new(
        1,
        (55, 52, 500, 500),        // kalshi: yes=55, no=52
        (45, 58, 500, 500),        // poly: yes=45, no=58
        &config,
        12345,
    );

    assert!(arb.is_valid());
    assert_eq!(arb.arb_type(), Some(ArbType::PolyYesKalshiNo));
    assert_eq!(arb.yes_price(), 45);  // poly yes
    assert_eq!(arb.no_price(), 52);   // kalshi no
}

#[test]
fn test_arb_opportunity_invalid_when_size_zero() {
    let config = ArbConfig::default();

    // Great prices but no size on kalshi NO side
    let arb = ArbOpportunity::new(
        1,
        (55, 52, 500, 0),          // kalshi: no_size = 0
        (45, 58, 500, 500),        // poly: good sizes
        &config,
        12345,
    );

    // Should be invalid because max_contracts < min_contracts
    assert!(!arb.is_valid());
}

#[test]
fn test_arb_opportunity_max_contracts() {
    let config = ArbConfig::default();

    let arb = ArbOpportunity::new(
        1,
        (55, 52, 300, 500),        // kalshi sizes: yes=300, no=500
        (45, 58, 400, 600),        // poly sizes: yes=400, no=600
        &config,
        12345,
    );

    // PolyYesKalshiNo: yes_size=400 (poly), no_size=500 (kalshi)
    // max_contracts = min(400, 500) / 100 = 4.0
    assert!(arb.is_valid());
    assert!((arb.max_contracts() - 4.0).abs() < 0.01);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p controller arb::tests::test_arb_opportunity`
Expected: FAIL - `ArbOpportunity` not found

**Step 3: Write minimal implementation**

Add to `controller/src/arb.rs`:

```rust
use crate::types::ArbType;

/// Centralized arbitrage opportunity detection.
///
/// Constructed with raw orderbook data, determines internally if a valid
/// arbitrage opportunity exists based on prices, sizes, fees, and thresholds.
#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    market_id: u16,
    arb_type: Option<ArbType>,
    yes_price: PriceCents,
    no_price: PriceCents,
    yes_size: SizeCents,
    no_size: SizeCents,
    detected_ns: u64,
}

impl ArbOpportunity {
    /// Create a new arb opportunity from raw orderbook data.
    ///
    /// Internally determines the best arb type (if any) and validates
    /// against the config thresholds.
    ///
    /// # Arguments
    /// * `market_id` - Market identifier
    /// * `kalshi` - Kalshi orderbook: (yes_ask, no_ask, yes_size, no_size)
    /// * `poly` - Polymarket orderbook: (yes_ask, no_ask, yes_size, no_size)
    /// * `config` - Arb configuration (threshold, min_contracts, fees)
    /// * `detected_ns` - Detection timestamp in nanoseconds
    pub fn new(
        market_id: u16,
        kalshi: (PriceCents, PriceCents, SizeCents, SizeCents),
        poly: (PriceCents, PriceCents, SizeCents, SizeCents),
        config: &ArbConfig,
        detected_ns: u64,
    ) -> Self {
        let (k_yes, k_no, k_yes_size, k_no_size) = kalshi;
        let (p_yes, p_no, p_yes_size, p_no_size) = poly;

        // Check for invalid prices (0 = no price available)
        if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
            return Self::invalid(market_id, detected_ns);
        }

        // Calculate Kalshi fees using the fee table formula
        let k_yes_fee = kalshi_fee(k_yes);
        let k_no_fee = kalshi_fee(k_no);

        // Calculate costs for each arb type (in priority order)
        let arb_types = [
            // Poly YES + Kalshi NO
            (
                ArbType::PolyYesKalshiNo,
                p_yes + k_no + k_no_fee,
                p_yes, k_no,
                p_yes_size, k_no_size,
            ),
            // Kalshi YES + Poly NO
            (
                ArbType::KalshiYesPolyNo,
                k_yes + k_yes_fee + p_no,
                k_yes, p_no,
                k_yes_size, p_no_size,
            ),
            // Poly only (both sides)
            (
                ArbType::PolyOnly,
                p_yes + p_no,
                p_yes, p_no,
                p_yes_size, p_no_size,
            ),
            // Kalshi only (both sides)
            (
                ArbType::KalshiOnly,
                k_yes + k_yes_fee + k_no + k_no_fee,
                k_yes, k_no,
                k_yes_size, k_no_size,
            ),
        ];

        // Find the best valid arb (lowest cost that beats threshold)
        for (arb_type, cost, yes_price, no_price, yes_size, no_size) in arb_types {
            if cost < config.threshold_cents {
                let max_contracts = (yes_size.min(no_size) as f64) / 100.0;

                if max_contracts >= config.min_contracts {
                    return Self {
                        market_id,
                        arb_type: Some(arb_type),
                        yes_price,
                        no_price,
                        yes_size,
                        no_size,
                        detected_ns,
                    };
                }
            }
        }

        Self::invalid(market_id, detected_ns)
    }

    /// Create an invalid (no arb) result
    fn invalid(market_id: u16, detected_ns: u64) -> Self {
        Self {
            market_id,
            arb_type: None,
            yes_price: 0,
            no_price: 0,
            yes_size: 0,
            no_size: 0,
            detected_ns,
        }
    }

    /// Check if this represents a valid arbitrage opportunity
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.arb_type.is_some()
    }

    /// Get the arbitrage type, if valid
    #[inline]
    pub fn arb_type(&self) -> Option<ArbType> {
        self.arb_type
    }

    /// Get the market ID
    #[inline]
    pub fn market_id(&self) -> u16 {
        self.market_id
    }

    /// Get the YES side price in cents
    #[inline]
    pub fn yes_price(&self) -> PriceCents {
        self.yes_price
    }

    /// Get the NO side price in cents
    #[inline]
    pub fn no_price(&self) -> PriceCents {
        self.no_price
    }

    /// Get the YES side size in cents
    #[inline]
    pub fn yes_size(&self) -> SizeCents {
        self.yes_size
    }

    /// Get the NO side size in cents
    #[inline]
    pub fn no_size(&self) -> SizeCents {
        self.no_size
    }

    /// Get the detection timestamp in nanoseconds
    #[inline]
    pub fn detected_ns(&self) -> u64 {
        self.detected_ns
    }

    /// Calculate max contracts available (minimum of both sides)
    #[inline]
    pub fn max_contracts(&self) -> f64 {
        (self.yes_size.min(self.no_size) as f64) / 100.0
    }

    /// Calculate gross profit in cents (before fees, for display)
    #[inline]
    pub fn gross_profit_cents(&self) -> PriceCents {
        100_u16.saturating_sub(self.yes_price + self.no_price)
    }
}

/// Calculate Kalshi trading fee in cents for a given price.
/// Formula: ceil(0.07 * price * (1 - price/100))
#[inline]
fn kalshi_fee(price_cents: PriceCents) -> PriceCents {
    if price_cents == 0 || price_cents >= 100 {
        return 0;
    }
    let p = price_cents as u32;
    let numerator = 7 * p * (100 - p) + 9999;
    (numerator / 10000) as PriceCents
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p controller arb::tests`
Expected: All PASS

**Step 5: Commit**

```bash
git add controller/src/arb.rs
git commit -m "$(cat <<'EOF'
feat(arb): add ArbOpportunity struct for centralized detection

ArbOpportunity encapsulates all arb detection logic:
- Calculates costs for all 4 arb types with fees
- Validates against threshold and min_contracts
- Returns invalid if prices missing or size insufficient

Callers construct with raw orderbook data and check is_valid().

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add ArbConfig to GlobalState

**Files:**
- Modify: `controller/src/types.rs`
- Modify: `controller/src/main.rs`

**Step 1: Write the failing test**

Add to `controller/src/types.rs` tests or create new test file:

```rust
#[test]
fn test_global_state_has_arb_config() {
    use crate::arb::ArbConfig;

    let config = ArbConfig::default();
    let state = GlobalState::new(config);

    assert_eq!(state.arb_config().threshold_cents, 99);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p controller types::tests::test_global_state_has_arb_config`
Expected: FAIL - `GlobalState::new` doesn't accept ArbConfig

**Step 3: Modify GlobalState to hold ArbConfig**

In `controller/src/types.rs`, modify `GlobalState`:

```rust
use crate::arb::ArbConfig;

pub struct GlobalState {
    // ... existing fields ...
    arb_config: ArbConfig,
}

impl GlobalState {
    pub fn new(arb_config: ArbConfig) -> Self {
        Self {
            // ... existing initialization ...
            arb_config,
        }
    }

    /// Get the arb configuration
    #[inline]
    pub fn arb_config(&self) -> &ArbConfig {
        &self.arb_config
    }
}
```

Update `main.rs` where `GlobalState::new()` is called:

```rust
let arb_config = ArbConfig::from_env();
let state = Arc::new(GlobalState::new(arb_config));
```

**Step 4: Run tests and cargo check**

Run: `cargo test -p controller && cargo check -p controller`
Expected: PASS

**Step 5: Commit**

```bash
git add controller/src/types.rs controller/src/main.rs
git commit -m "$(cat <<'EOF'
refactor(types): add ArbConfig to GlobalState

GlobalState now holds ArbConfig, built from env at startup.
This makes arb configuration available throughout the system
without passing it explicitly to every function.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Replace check_arbs + send_arb_request in polymarket.rs

**Files:**
- Modify: `controller/src/polymarket.rs`

**Step 1: Identify call sites**

Search for `check_arbs` and `send_arb_request` calls in `polymarket.rs`. These should be replaced with:

```rust
let arb = ArbOpportunity::new(
    market_id,
    market.kalshi.load(),
    market.poly.load(),
    state.arb_config(),
    clock.now_ns(),
);

if arb.is_valid() {
    // Route to confirm or exec channel based on league
}
```

**Step 2: Write the replacement code**

Replace `send_arb_request` function entirely. Where `check_arbs` was called:

Before:
```rust
let arb_mask = market.check_arbs(threshold_cents);
if arb_mask != 0 {
    send_arb_request(state, market_id, market, arb_mask, exec_tx, confirm_tx, clock).await;
}
```

After:
```rust
use crate::arb::ArbOpportunity;

let arb = ArbOpportunity::new(
    market_id,
    market.kalshi.load(),
    market.poly.load(),
    state.arb_config(),
    clock.now_ns(),
);

if arb.is_valid() {
    route_arb_to_channel(state, market_id, arb, exec_tx, confirm_tx).await;
}
```

Create helper function `route_arb_to_channel`:

```rust
/// Route a valid arb opportunity to the appropriate channel
async fn route_arb_to_channel(
    state: &GlobalState,
    market_id: u16,
    arb: ArbOpportunity,
    exec_tx: &mpsc::Sender<FastExecutionRequest>,
    confirm_tx: &mpsc::Sender<(FastExecutionRequest, Arc<MarketPair>)>,
) {
    let req = FastExecutionRequest::from_arb(&arb);

    let pair = match state.get_by_id(market_id).and_then(|m| m.pair()) {
        Some(p) => p,
        None => {
            if let Err(e) = exec_tx.try_send(req) {
                tracing::warn!("[POLY] Arb request dropped: {} (channel backpressure)", e);
            }
            return;
        }
    };

    if config::requires_confirmation(&pair.league) {
        if let Err(e) = confirm_tx.try_send((req, pair)) {
            tracing::warn!("[POLY] Confirm request dropped: {} (channel backpressure)", e);
        }
    } else {
        if let Err(e) = exec_tx.try_send(req) {
            tracing::warn!("[POLY] Arb request dropped: {} (channel backpressure)", e);
        }
    }
}
```

**Step 3: Add FastExecutionRequest::from_arb helper**

In `controller/src/types.rs`:

```rust
impl FastExecutionRequest {
    /// Create from an ArbOpportunity (must be valid)
    pub fn from_arb(arb: &crate::arb::ArbOpportunity) -> Self {
        Self {
            market_id: arb.market_id(),
            yes_price: arb.yes_price(),
            no_price: arb.no_price(),
            yes_size: arb.yes_size(),
            no_size: arb.no_size(),
            arb_type: arb.arb_type().expect("from_arb called on invalid arb"),
            detected_ns: arb.detected_ns(),
            is_test: false,
        }
    }
}
```

**Step 4: Run tests**

Run: `cargo test -p controller && cargo check -p controller`
Expected: PASS

**Step 5: Commit**

```bash
git add controller/src/polymarket.rs controller/src/types.rs
git commit -m "$(cat <<'EOF'
refactor(poly): use ArbOpportunity for arb detection

Replace check_arbs + send_arb_request with ArbOpportunity::new().
This centralizes all arb logic and eliminates the size=0 arb bug
where arbs were detected by price alone.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Replace check_arbs + send_kalshi_arb_request in kalshi.rs

**Files:**
- Modify: `controller/src/kalshi.rs`

**Step 1: Apply same pattern as Task 4**

Replace all `check_arbs` + `send_kalshi_arb_request` call sites with `ArbOpportunity::new()`.

**Step 2: Remove send_kalshi_arb_request function**

The function is no longer needed - delete it entirely.

**Step 3: Run tests**

Run: `cargo test -p controller && cargo check -p controller`
Expected: PASS

**Step 4: Commit**

```bash
git add controller/src/kalshi.rs
git commit -m "$(cat <<'EOF'
refactor(kalshi): use ArbOpportunity for arb detection

Replace check_arbs + send_kalshi_arb_request with ArbOpportunity::new().
Eliminates duplicate arb construction logic that existed in both
kalshi.rs and polymarket.rs.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Update confirm_queue.rs to use ArbOpportunity

**Files:**
- Modify: `controller/src/confirm_queue.rs`

This is where the size=0 bug surfaced. The confirm queue has its own validation logic that duplicates arb detection.

**Step 1: Identify duplicated logic**

Current problems in `confirm_queue.rs`:
- `push()` line 112-116: Manual size check (`min_size < 100`) duplicates min_contracts
- `validate_arb_detailed()`: Recalculates costs with `kalshi_fee_cents` - duplicates fee logic
- Uses `kalshi_fee_cents` from types.rs instead of centralized fee calculation

**Step 2: Remove size check from push()**

The size check in `push()` should be removed - if an `ArbOpportunity` reached here, it's already validated. The check was a band-aid for the upstream problem.

Before:
```rust
pub async fn push(&self, request: FastExecutionRequest, pair: Arc<MarketPair>) -> bool {
    let market_id = request.market_id;

    // Skip arbs with less than 1 contract of liquidity
    let min_size = request.yes_size.min(request.no_size);
    if min_size < 100 {
        return false;
    }
    // ...
}
```

After:
```rust
pub async fn push(&self, request: FastExecutionRequest, pair: Arc<MarketPair>) -> bool {
    let market_id = request.market_id;
    // Size validation now happens at detection time in ArbOpportunity::new()
    // ...
}
```

**Step 3: Refactor validate_arb_detailed to use ArbOpportunity**

The validation should re-check using `ArbOpportunity::new()` with current prices:

```rust
use crate::arb::ArbOpportunity;

/// Check if arb is still valid by re-detecting with current prices
pub fn validate_arb(&self, arb: &PendingArb) -> bool {
    // Test arbs use synthetic prices, skip validation
    if arb.request.is_test {
        return true;
    }

    let market = match self.state.get_by_id(arb.request.market_id) {
        Some(m) => m,
        None => return false,
    };

    // Re-detect arb with current orderbook prices
    let current_arb = ArbOpportunity::new(
        arb.request.market_id,
        market.kalshi.load(),
        market.poly.load(),
        self.state.arb_config(),
        0, // timestamp not needed for validation
    );

    // Valid if same arb type is still profitable
    current_arb.arb_type() == Some(arb.request.arb_type)
}
```

**Step 4: Update validate_arb_detailed for TUI display**

Keep `validate_arb_detailed` for TUI to show cost changes, but use centralized fee calculation:

```rust
use crate::arb::kalshi_fee;  // Use the centralized fee function

pub fn validate_arb_detailed(&self, arb: &PendingArb) -> Option<ValidationResult> {
    // ... test arb check ...

    let market = self.state.get_by_id(arb.request.market_id)?;
    let (k_yes, k_no, _, _) = market.kalshi.load();
    let (p_yes, p_no, _, _) = market.poly.load();

    // Use centralized fee calculation from arb module
    let original_cost = arb.request.yes_price + arb.request.no_price + match arb.request.arb_type {
        ArbType::PolyYesKalshiNo => kalshi_fee(arb.request.no_price),
        ArbType::KalshiYesPolyNo => kalshi_fee(arb.request.yes_price),
        ArbType::PolyOnly => 0,
        ArbType::KalshiOnly => kalshi_fee(arb.request.yes_price) + kalshi_fee(arb.request.no_price),
    };

    // ... rest of validation ...
}
```

**Step 5: Export kalshi_fee from arb module**

In `controller/src/arb.rs`, make the fee function public:

```rust
/// Calculate Kalshi trading fee in cents for a given price.
/// Formula: ceil(0.07 * price * (1 - price/100))
#[inline]
pub fn kalshi_fee(price_cents: PriceCents) -> PriceCents {
    // ... implementation ...
}
```

**Step 6: Remove kalshi_fee_cents from types.rs**

After confirm_queue.rs uses `arb::kalshi_fee`, remove the duplicate from types.rs.

**Step 7: Run tests**

Run: `cargo test -p controller && cargo check -p controller`
Expected: PASS

**Step 8: Commit**

```bash
git add controller/src/confirm_queue.rs controller/src/arb.rs controller/src/types.rs
git commit -m "$(cat <<'EOF'
refactor(confirm): use ArbOpportunity for arb validation

- Remove redundant size check in push() (validated at detection)
- Use ArbOpportunity::new() for re-validation with current prices
- Centralize kalshi_fee calculation in arb module
- Remove duplicate kalshi_fee_cents from types.rs

This fixes the size=0 arb bug that surfaced in confirm mode.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Update main.rs startup sweep and heartbeat

**Files:**
- Modify: `controller/src/main.rs`

**Step 1: Find check_arbs calls in main.rs**

Look for heartbeat monitor and startup sweep code that uses `check_arbs`.

**Step 2: Replace with ArbOpportunity**

The startup sweep and heartbeat should use the same pattern:

```rust
let arb = ArbOpportunity::new(
    market_id,
    market.kalshi.load(),
    market.poly.load(),
    state.arb_config(),
    clock.now_ns(),
);

if arb.is_valid() {
    // Handle arb
}
```

**Step 3: Run tests**

Run: `cargo test -p controller && cargo check -p controller`
Expected: PASS

**Step 4: Commit**

```bash
git add controller/src/main.rs
git commit -m "$(cat <<'EOF'
refactor(main): use ArbOpportunity in startup sweep and heartbeat

All arb detection now goes through ArbOpportunity::new(), ensuring
consistent validation of prices, sizes, and thresholds.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Remove check_arbs from AtomicMarketState

**Files:**
- Modify: `controller/src/types.rs`

**Step 1: Verify no remaining callers**

Run: `grep -r "check_arbs" controller/src/`
Expected: Only the definition in types.rs, no callers

**Step 2: Delete check_arbs method**

Remove the `check_arbs` method from `AtomicMarketState` and the `KALSHI_FEE_TABLE` static (moved to arb.rs).

**Step 3: Run tests**

Run: `cargo test -p controller && cargo check -p controller`
Expected: PASS

**Step 4: Commit**

```bash
git add controller/src/types.rs
git commit -m "$(cat <<'EOF'
refactor(types): remove deprecated check_arbs method

All arb detection now uses ArbOpportunity. The check_arbs method
and KALSHI_FEE_TABLE have been removed from types.rs as they're
now in arb.rs.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Update CLAUDE.md documentation

**Files:**
- Modify: `CLAUDE.md`

**Step 1: Add ArbOpportunity to architecture section**

Document the new arb detection flow and ArbConfig environment variables.

**Step 2: Update environment variables section**

Add:
- `ARB_THRESHOLD_CENTS` - threshold in cents (default: 99)
- `ARB_MIN_CONTRACTS` - minimum contracts for valid arb (default: 1.0)

**Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'EOF'
docs: update CLAUDE.md with ArbOpportunity architecture

Document the centralized arb detection through ArbOpportunity
and new environment variables for arb configuration.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Final integration test

**Files:**
- Review all changes

**Step 1: Run full test suite**

Run: `cargo test -p controller`
Expected: All PASS

**Step 2: Run with dry run to verify behavior**

Run: `DRY_RUN=1 dotenvx run -- cargo run -p controller --release`
Expected: No panics, arb detection works, no size=0 arbs logged

**Step 3: Verify no arbs with size 0 reach execution**

The "Insufficient liquidity" warning should no longer appear for size=0 cases,
as they're filtered at detection time.

---

## Summary

After completing all tasks:

1. **ArbConfig** - Centralized configuration loaded from env at startup
2. **ArbOpportunity** - Single struct that owns all arb detection logic
3. **No duplicate code** - Removed send_arb_request and send_kalshi_arb_request
4. **No size=0 arbs** - Filtered at detection time, not execution time
5. **Clean architecture** - Callers pass raw data, struct determines validity
