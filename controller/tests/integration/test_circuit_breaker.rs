//! Integration tests for the circuit breaker system.
//!
//! These tests verify that the circuit breaker correctly:
//! 1. Allows trades within limits
//! 2. Blocks trades that exceed position limits
//! 3. Tracks positions per market and total
//! 4. Handles consecutive errors
//! 5. Manages cooldown periods
//! 6. Calculates remaining capacity
//!
//! NOTE: All position limits are in DOLLARS (cost basis).
//! We use 50c prices throughout so that each leg costs contracts * 0.50.
//! Total cost basis per record_success = kalshi_contracts*0.50 + poly_contracts*0.50.

use arb_bot::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, TripReason};

// ============================================================================
// TEST HELPERS
// ============================================================================

/// Create a circuit breaker with standard test configuration.
/// Limits are in dollars. At 50c prices: 100 contracts per leg = $50 per leg.
fn create_test_circuit_breaker() -> CircuitBreaker {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,  // $50 (was 100 contracts)
        max_total_position: 250.0,      // $250 (was 500 contracts)
        max_daily_loss: 100.0,
        max_consecutive_errors: 5,
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 1,
    };
    CircuitBreaker::new(config)
}

/// Create a circuit breaker with custom per-market and total limits (in dollars).
fn create_circuit_breaker_with_limits(
    per_market: f64,
    total: f64,
    daily_loss: f64,
) -> CircuitBreaker {
    let config = CircuitBreakerConfig {
        max_position_per_market: per_market,
        max_total_position: total,
        max_daily_loss: daily_loss,
        max_consecutive_errors: 5,
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 1,
    };
    CircuitBreaker::new(config)
}

/// Create a disabled circuit breaker.
fn create_disabled_circuit_breaker() -> CircuitBreaker {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 5,
        cooldown_secs: 60,
        enabled: false,
        min_contracts: 1,
    };
    CircuitBreaker::new(config)
}

/// Create a circuit breaker with very short cooldown for testing.
fn create_circuit_breaker_with_short_cooldown() -> CircuitBreaker {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 3,
        cooldown_secs: 1, // 1 second cooldown for testing
        enabled: true,
        min_contracts: 1,
    };
    CircuitBreaker::new(config)
}

// ============================================================================
// TEST: ALLOWS WITHIN LIMITS
// ============================================================================

/// Test that trades within all limits are allowed.
#[tokio::test]
async fn test_circuit_breaker_allows_within_limits() {
    let cb = create_test_circuit_breaker();

    // Should allow trade within limits (cost_basis $25)
    let result = cb.can_execute("KXNBA-26-SAS", 25.0).await;
    assert!(result.is_ok(), "Trade within limits should be allowed");

    // Should allow trade for different market
    let result = cb.can_execute("KXNFL-26-DEN", 25.0).await;
    assert!(
        result.is_ok(),
        "Trade for different market should be allowed"
    );
}

/// Test that multiple small trades within limits are allowed.
#[tokio::test]
async fn test_circuit_breaker_allows_multiple_small_trades() {
    let cb = create_test_circuit_breaker();

    // Record several successful trades: 20 contracts per leg @ 50c = $10 + $10 = $20 per trade
    cb.record_success("market1", 20, 50, 20, 50, 0.0).await;
    cb.record_success("market2", 20, 50, 20, 50, 0.0).await;
    cb.record_success("market3", 20, 50, 20, 50, 0.0).await;

    // Total is now $60 across 3 markets ($20 each)
    // Total limit is $250, should still allow trades
    let result = cb.can_execute("market4", 25.0).await;
    assert!(
        result.is_ok(),
        "Should allow trade when within total limit"
    );
}

// ============================================================================
// TEST: BLOCKS MAX POSITION PER MARKET
// ============================================================================

/// Test that exceeding per-market limit triggers MaxPositionPerMarket trip.
#[tokio::test]
async fn test_circuit_breaker_blocks_max_position_per_market() {
    // Per-market limit: $50, total: $500
    let cb = create_circuit_breaker_with_limits(50.0, 500.0, 100.0);

    // Record 40 contracts per leg @ 50c = $20 + $20 = $40 cost basis for market1
    cb.record_success("market1", 40, 50, 40, 50, 0.0).await;

    // Try to add $15 more (would be $55 total, exceeds $50 limit)
    let result = cb.can_execute("market1", 15.0).await;

    match result {
        Err(TripReason::MaxPositionPerMarket {
            market,
            position,
            limit,
        }) => {
            assert_eq!(market, "market1");
            assert!((position - 55.0).abs() < 0.01, "Expected position ~55.0, got {}", position);
            assert!((limit - 50.0).abs() < 0.01, "Expected limit ~50.0, got {}", limit);
        }
        other => panic!(
            "Expected MaxPositionPerMarket error, got {:?}",
            other
        ),
    }
}

/// Test that per-market limit is enforced even for new markets.
#[tokio::test]
async fn test_circuit_breaker_blocks_new_market_exceeding_limit() {
    // Per-market limit: $25
    let cb = create_circuit_breaker_with_limits(25.0, 500.0, 100.0);

    // Try to add $30 to a new market (exceeds $25 limit)
    let result = cb.can_execute("new_market", 30.0).await;

    match result {
        Err(TripReason::MaxPositionPerMarket {
            market,
            position,
            limit,
        }) => {
            assert_eq!(market, "new_market");
            assert!((position - 30.0).abs() < 0.01, "Expected position ~30.0, got {}", position);
            assert!((limit - 25.0).abs() < 0.01, "Expected limit ~25.0, got {}", limit);
        }
        other => panic!(
            "Expected MaxPositionPerMarket error, got {:?}",
            other
        ),
    }

    // But a trade within the limit should be allowed
    let result = cb.can_execute("new_market", 20.0).await;
    assert!(
        result.is_ok(),
        "Trade within limit should be allowed for new market"
    );
}

// ============================================================================
// TEST: BLOCKS MAX TOTAL POSITION
// ============================================================================

/// Test that exceeding total position limit triggers MaxTotalPosition trip.
#[tokio::test]
async fn test_circuit_breaker_blocks_max_total_position() {
    // Per-market: $100, total: $50
    let cb = create_circuit_breaker_with_limits(100.0, 50.0, 100.0);

    // Record 30 contracts per leg @ 50c = $15 + $15 = $30 cost basis
    cb.record_success("market1", 30, 50, 30, 50, 0.0).await;

    // Try to add $25 more (would be $55 total, exceeds $50 limit)
    let result = cb.can_execute("market2", 25.0).await;

    match result {
        Err(TripReason::MaxTotalPosition { position, limit }) => {
            assert!((position - 55.0).abs() < 0.01, "Expected position ~55.0, got {}", position);
            assert!((limit - 50.0).abs() < 0.01, "Expected limit ~50.0, got {}", limit);
        }
        other => panic!("Expected MaxTotalPosition error, got {:?}", other),
    }
}

/// Test total position limit across many markets.
#[tokio::test]
async fn test_circuit_breaker_blocks_total_across_many_markets() {
    // Per-market: $50, total: $100
    let cb = create_circuit_breaker_with_limits(50.0, 100.0, 100.0);

    // Record trades across multiple markets:
    // market1: 40 per leg @ 50c = $40 cost basis
    cb.record_success("market1", 40, 50, 40, 50, 0.0).await;
    // market2: 40 per leg @ 50c = $40 cost basis
    cb.record_success("market2", 40, 50, 40, 50, 0.0).await;
    // Total: $80

    // Try to add $25 more to a new market (would be $105 total, exceeds $100 limit)
    let result = cb.can_execute("market3", 25.0).await;

    match result {
        Err(TripReason::MaxTotalPosition { position, limit }) => {
            assert!((position - 105.0).abs() < 0.01, "Expected position ~105.0, got {}", position);
            assert!((limit - 100.0).abs() < 0.01, "Expected limit ~100.0, got {}", limit);
        }
        other => panic!("Expected MaxTotalPosition error, got {:?}", other),
    }
}

// ============================================================================
// TEST: BLOCKS MAX DAILY LOSS
// ============================================================================

/// Test that exceeding daily loss triggers MaxDailyLoss trip.
#[tokio::test]
async fn test_circuit_breaker_blocks_max_daily_loss() {
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 50.0);

    // Record a large loss
    cb.record_pnl(-60.0); // $60 loss exceeds $50 limit

    // Try to execute a trade
    let result = cb.can_execute("market1", 5.0).await;

    match result {
        Err(TripReason::MaxDailyLoss { loss, limit }) => {
            assert!((loss - 60.0).abs() < 0.01, "Loss should be $60");
            assert!((limit - 50.0).abs() < 0.01, "Limit should be $50");
        }
        other => panic!("Expected MaxDailyLoss error, got {:?}", other),
    }
}

/// Test daily loss accumulates from multiple trades.
#[tokio::test]
async fn test_circuit_breaker_daily_loss_accumulates() {
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 50.0);

    // Record multiple small losses (10 contracts per leg @ 50c = $10 cost basis per trade)
    cb.record_success("market1", 10, 50, 10, 50, -15.0).await;
    cb.record_success("market2", 10, 50, 10, 50, -20.0).await;
    cb.record_success("market3", 10, 50, 10, 50, -20.0).await;
    // Total loss: $55

    // Try to execute a trade
    let result = cb.can_execute("market4", 5.0).await;

    assert!(
        matches!(result, Err(TripReason::MaxDailyLoss { .. })),
        "Should block due to accumulated daily loss"
    );
}

// ============================================================================
// TEST: CONSECUTIVE ERRORS
// ============================================================================

/// Test that consecutive errors trigger ConsecutiveErrors trip.
#[tokio::test]
async fn test_circuit_breaker_consecutive_errors() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 3,
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 1,
    };
    let cb = CircuitBreaker::new(config);

    // Record 2 errors - should not trip yet
    cb.record_error().await;
    cb.record_error().await;
    assert!(
        cb.is_trading_allowed(),
        "Should still allow trading after 2 errors"
    );

    // Third error should trip
    cb.record_error().await;
    assert!(
        !cb.is_trading_allowed(),
        "Should halt trading after 3 errors"
    );

    // Verify the trip reason
    let result = cb.can_execute("market1", 5.0).await;
    match result {
        Err(TripReason::ConsecutiveErrors { count, limit }) => {
            assert_eq!(count, 3);
            assert_eq!(limit, 3);
        }
        other => panic!("Expected ConsecutiveErrors error, got {:?}", other),
    }
}

/// Test that success resets consecutive error count.
#[tokio::test]
async fn test_circuit_breaker_success_resets_errors() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 3,
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 1,
    };
    let cb = CircuitBreaker::new(config);

    // Record 2 errors
    cb.record_error().await;
    cb.record_error().await;

    // Record a success (should reset error count)
    cb.record_success("market1", 10, 50, 10, 50, 0.0).await;

    // Record 2 more errors - should not trip (count reset to 0)
    cb.record_error().await;
    cb.record_error().await;
    assert!(
        cb.is_trading_allowed(),
        "Should still allow trading after reset"
    );
}

// ============================================================================
// TEST: REMAINING CAPACITY
// ============================================================================

/// Test get_remaining_capacity with no positions.
#[tokio::test]
async fn test_circuit_breaker_remaining_capacity_empty() {
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 100.0);

    let capacity = cb.get_remaining_capacity("market1").await;

    assert!((capacity.per_market - 50.0).abs() < 0.01, "Per-market should equal limit");
    assert!((capacity.total - 250.0).abs() < 0.01, "Total should equal limit");
    assert!(
        (capacity.effective - 50.0).abs() < 0.01,
        "Effective should be min(50, 250)"
    );
}

/// Test get_remaining_capacity after recording a trade.
#[tokio::test]
async fn test_circuit_breaker_remaining_capacity_after_trade() {
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 100.0);

    // Record a trade: 15 contracts per leg @ 50c = $7.50 + $7.50 = $15 cost basis
    cb.record_success("market1", 15, 50, 15, 50, 0.0).await;

    // Check capacity for market1
    let capacity = cb.get_remaining_capacity("market1").await;
    assert!((capacity.per_market - 35.0).abs() < 0.01, "Per-market: 50 - 15 = 35");
    assert!((capacity.total - 235.0).abs() < 0.01, "Total: 250 - 15 = 235");
    assert!((capacity.effective - 35.0).abs() < 0.01, "Effective: min(35, 235) = 35");

    // Check capacity for a different market
    let capacity2 = cb.get_remaining_capacity("market2").await;
    assert!(
        (capacity2.per_market - 50.0).abs() < 0.01,
        "Different market has full per-market limit"
    );
    assert!((capacity2.total - 235.0).abs() < 0.01, "Total is shared across markets");
    assert!(
        (capacity2.effective - 50.0).abs() < 0.01,
        "Effective: min(50, 235) = 50"
    );
}

/// Test remaining capacity when total limit constrains more than per-market.
#[tokio::test]
async fn test_circuit_breaker_remaining_capacity_total_constrains() {
    let cb = create_circuit_breaker_with_limits(50.0, 25.0, 100.0);

    let capacity = cb.get_remaining_capacity("market1").await;

    assert!((capacity.per_market - 50.0).abs() < 0.01);
    assert!((capacity.total - 25.0).abs() < 0.01);
    assert!(
        (capacity.effective - 25.0).abs() < 0.01,
        "Effective constrained by total (25 < 50)"
    );
}

/// Test remaining capacity goes to zero when limits reached.
#[tokio::test]
async fn test_circuit_breaker_remaining_capacity_at_limit() {
    // Per-market limit: $50
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 100.0);

    // Fill up market1 to its limit: 50 per leg @ 50c = $25 + $25 = $50
    cb.record_success("market1", 50, 50, 50, 50, 0.0).await;

    let capacity = cb.get_remaining_capacity("market1").await;
    assert!((capacity.per_market).abs() < 0.01, "Market at limit should have 0 remaining");
    assert!((capacity.effective).abs() < 0.01, "Effective should be 0");
}

// ============================================================================
// TEST: COOLDOWN
// ============================================================================

/// Test that trades are blocked during cooldown period.
#[tokio::test]
async fn test_circuit_breaker_cooldown_blocks_trades() {
    let cb = create_circuit_breaker_with_short_cooldown();

    // Trip the circuit breaker
    cb.trip(TripReason::ManualHalt).await;

    // Should be halted
    assert!(!cb.is_trading_allowed(), "Should be halted after trip");

    // Trades should be blocked
    let result = cb.can_execute("market1", 5.0).await;
    assert!(
        matches!(result, Err(TripReason::ManualHalt)),
        "Trades should be blocked during cooldown"
    );
}

/// Test that cooldown auto-resets after duration.
#[tokio::test]
async fn test_circuit_breaker_cooldown_auto_resets() {
    let cb = create_circuit_breaker_with_short_cooldown();

    // Trip the circuit breaker
    cb.trip(TripReason::ManualHalt).await;
    assert!(!cb.is_trading_allowed(), "Should be halted after trip");

    // Wait for cooldown (1 second + buffer)
    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

    // Check cooldown - should auto-reset
    let allowed = cb.check_cooldown().await;
    assert!(allowed, "Should auto-reset after cooldown");
    assert!(
        cb.is_trading_allowed(),
        "Trading should be allowed after cooldown"
    );
}

// ============================================================================
// TEST: RESET
// ============================================================================

/// Test that reset allows trading again.
#[tokio::test]
async fn test_circuit_breaker_reset() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 3,
        cooldown_secs: 3600, // Long cooldown so we test manual reset
        enabled: true,
        min_contracts: 1,
    };
    let cb = CircuitBreaker::new(config);

    // Trip via consecutive errors
    cb.record_error().await;
    cb.record_error().await;
    cb.record_error().await;
    assert!(!cb.is_trading_allowed(), "Should be halted");

    // Reset
    cb.reset().await;

    // Should allow trading again
    assert!(cb.is_trading_allowed(), "Should allow trading after reset");

    let result = cb.can_execute("market1", 5.0).await;
    assert!(result.is_ok(), "Trade should be allowed after reset");
}

/// Test that reset clears consecutive error count.
#[tokio::test]
async fn test_circuit_breaker_reset_clears_error_count() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 3,
        cooldown_secs: 3600,
        enabled: true,
        min_contracts: 1,
    };
    let cb = CircuitBreaker::new(config);

    // Trip via consecutive errors
    cb.record_error().await;
    cb.record_error().await;
    cb.record_error().await;

    // Reset
    cb.reset().await;

    // Can now accumulate errors again from 0
    cb.record_error().await;
    cb.record_error().await;
    assert!(
        cb.is_trading_allowed(),
        "Should still be trading after 2 errors post-reset"
    );
}

// ============================================================================
// TEST: DISABLED
// ============================================================================

/// Test that disabled circuit breaker allows all trades.
#[tokio::test]
async fn test_circuit_breaker_disabled() {
    let cb = create_disabled_circuit_breaker();

    // Should allow trade that would exceed limits if enabled
    let result = cb.can_execute("market1", 500.0).await;
    assert!(
        result.is_ok(),
        "Disabled CB should allow trades exceeding limits"
    );
}

/// Test that disabled circuit breaker reports unlimited capacity.
#[tokio::test]
async fn test_circuit_breaker_disabled_capacity() {
    let cb = create_disabled_circuit_breaker();

    let capacity = cb.get_remaining_capacity("market1").await;

    assert_eq!(capacity.per_market, f64::MAX, "Should report unlimited");
    assert_eq!(capacity.total, f64::MAX, "Should report unlimited");
    assert_eq!(capacity.effective, f64::MAX, "Should report unlimited");
}

/// Test that disabled circuit breaker ignores errors.
#[tokio::test]
async fn test_circuit_breaker_disabled_ignores_errors() {
    let cb = create_disabled_circuit_breaker();

    // Record many errors
    for _ in 0..10 {
        cb.record_error().await;
    }

    // Should still allow trading
    assert!(
        cb.is_trading_allowed(),
        "Disabled CB should allow trading despite errors"
    );
}

/// Test that disabled circuit breaker ignores daily loss.
#[tokio::test]
async fn test_circuit_breaker_disabled_ignores_daily_loss() {
    let cb = create_disabled_circuit_breaker();

    // Record huge loss
    cb.record_pnl(-10000.0);

    // Should still allow trading
    let result = cb.can_execute("market1", 5.0).await;
    assert!(
        result.is_ok(),
        "Disabled CB should allow trading despite losses"
    );
}

// ============================================================================
// TEST: CAPS TO REMAINING CAPACITY
// ============================================================================

/// Test that trades should be capped to remaining capacity.
///
/// Note: The circuit breaker provides `get_remaining_capacity()` which
/// the caller uses to cap trade sizes. The circuit breaker itself
/// doesn't cap trades - it just reports capacity.
#[tokio::test]
async fn test_circuit_breaker_caps_to_remaining() {
    // Per-market: $50
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 100.0);

    // Record 35 contracts per leg @ 50c = $17.50 + $17.50 = $35 cost basis
    cb.record_success("market1", 35, 50, 35, 50, 0.0).await;

    // Get remaining capacity
    let capacity = cb.get_remaining_capacity("market1").await;
    assert!((capacity.per_market - 15.0).abs() < 0.01, "Should have $15 remaining for market1");

    // Simulate what the caller would do: cap the trade to remaining capacity
    let requested_cost = 25.0;
    let capped_cost = if requested_cost < capacity.effective { requested_cost } else { capacity.effective };
    assert!((capped_cost - 15.0).abs() < 0.01, "Trade should be capped to $15");

    // Verify the capped trade would be allowed
    let result = cb.can_execute("market1", capped_cost).await;
    assert!(result.is_ok(), "Capped trade should be allowed");
}

/// Test capacity calculation with both per-market and total constraints.
#[tokio::test]
async fn test_circuit_breaker_capacity_dual_constraints() {
    // Per-market: $25, total: $50
    let cb = create_circuit_breaker_with_limits(25.0, 50.0, 100.0);

    // Fill up $20 per market across 2 markets:
    // 20 per leg @ 50c = $10 + $10 = $20 cost basis per market
    cb.record_success("market1", 20, 50, 20, 50, 0.0).await;
    cb.record_success("market2", 20, 50, 20, 50, 0.0).await;
    // Total: $40

    // Check capacity for market3
    let capacity = cb.get_remaining_capacity("market3").await;
    assert!((capacity.per_market - 25.0).abs() < 0.01, "Full per-market limit available");
    assert!((capacity.total - 10.0).abs() < 0.01, "Only $10 total remaining");
    assert!(
        (capacity.effective - 10.0).abs() < 0.01,
        "Effective constrained by total ($10 < $25)"
    );
}

// ============================================================================
// TEST: MIN CONTRACTS
// ============================================================================

/// Test that min_contracts getter returns configured value.
#[tokio::test]
async fn test_circuit_breaker_min_contracts() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 5,
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 5,
    };
    let cb = CircuitBreaker::new(config);

    assert_eq!(cb.min_contracts(), 5);
}

/// Test that min_contracts is used for filtering (caller responsibility).
#[tokio::test]
async fn test_circuit_breaker_min_contracts_filtering() {
    let cb = create_test_circuit_breaker();

    // Get remaining capacity
    let capacity = cb.get_remaining_capacity("market1").await;
    let min = cb.min_contracts();

    // Simulate what caller would do: skip if effective < min (comparing dollars vs contracts)
    let should_trade = capacity.effective >= min as f64;
    assert!(
        should_trade,
        "Should trade when capacity >= min_contracts"
    );

    // Simulate low capacity scenario: per-market $1.5
    let cb2 = create_circuit_breaker_with_limits(1.5, 250.0, 100.0);
    // Record 1 contract per leg @ 50c = $0.50 + $0.50 = $1.00
    cb2.record_success("market1", 1, 50, 1, 50, 0.0).await;

    let capacity2 = cb2.get_remaining_capacity("market1").await;
    assert!((capacity2.per_market - 0.5).abs() < 0.01);

    // With min_contracts = 1, this would still allow trading
    // But if min_contracts was higher, caller would skip
}

// ============================================================================
// TEST: STATUS
// ============================================================================

/// Test circuit breaker status reporting.
#[tokio::test]
async fn test_circuit_breaker_status() {
    let cb = create_test_circuit_breaker();

    // Record some activity: 10 per leg @ 50c = $5+$5=$10, 15 per leg @ 50c = $7.5+$7.5=$15
    cb.record_success("market1", 10, 50, 10, 50, 5.0).await;
    cb.record_success("market2", 15, 50, 15, 50, -3.0).await;

    let status = cb.status().await;

    assert!(status.enabled, "Should be enabled");
    assert!(!status.halted, "Should not be halted");
    assert!(status.trip_reason.is_none(), "Should have no trip reason");
    assert_eq!(status.consecutive_errors, 0, "Should have no errors");
    assert!(
        (status.daily_pnl - 2.0).abs() < 0.01,
        "P&L should be $5 - $3 = $2"
    );
    // Total position: $10 + $15 = $25 in dollars
    assert!(
        (status.total_position - 25.0).abs() < 0.01,
        "Total: $10 + $15 = $25"
    );
    assert_eq!(status.market_count, 2, "Two markets");
}

/// Test status after trip.
#[tokio::test]
async fn test_circuit_breaker_status_after_trip() {
    let cb = create_test_circuit_breaker();

    cb.trip(TripReason::ManualHalt).await;

    let status = cb.status().await;

    assert!(status.halted, "Should be halted");
    assert_eq!(
        status.trip_reason,
        Some(TripReason::ManualHalt),
        "Should have ManualHalt reason"
    );
}

// ============================================================================
// TEST: DAILY PNL RESET
// ============================================================================

/// Test daily P&L reset.
#[tokio::test]
async fn test_circuit_breaker_daily_pnl_reset() {
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 50.0);

    // Accumulate some P&L
    cb.record_success("market1", 10, 50, 10, 50, -30.0).await;
    cb.record_success("market2", 10, 50, 10, 50, -15.0).await;
    // Total loss: $45 (below $50 limit)

    let status_before = cb.status().await;
    assert!(
        (status_before.daily_pnl - (-45.0)).abs() < 0.01,
        "P&L should be -$45"
    );

    // Reset daily P&L
    cb.reset_daily_pnl();

    let status_after = cb.status().await;
    assert!(
        (status_after.daily_pnl).abs() < 0.01,
        "P&L should be $0 after reset"
    );

    // Should now allow trading again (loss limit no longer exceeded)
    let result = cb.can_execute("market3", 5.0).await;
    assert!(result.is_ok(), "Should allow trading after P&L reset");
}

// ============================================================================
// TEST: MANUAL HALT
// ============================================================================

/// Test manual halt functionality.
#[tokio::test]
async fn test_circuit_breaker_manual_halt() {
    let cb = create_test_circuit_breaker();

    assert!(cb.is_trading_allowed(), "Should initially allow trading");

    // Manually halt
    cb.halt().await;

    assert!(!cb.is_trading_allowed(), "Should be halted after manual halt");

    let result = cb.can_execute("market1", 5.0).await;
    match result {
        Err(TripReason::ManualHalt) => {} // Expected
        other => panic!("Expected ManualHalt error, got {:?}", other),
    }
}

// ============================================================================
// TEST: PER-MARKET BLACKLISTING
// ============================================================================

/// Create a circuit breaker with low blacklist threshold for testing.
fn create_circuit_breaker_with_blacklist_threshold(threshold: u32) -> CircuitBreaker {
    // Set env var before construction (CircuitBreaker reads it in new())
    std::env::set_var("CB_MARKET_BLACKLIST_THRESHOLD", threshold.to_string());
    std::env::set_var("CB_MARKET_BLACKLIST_SECS", "1"); // 1s for fast tests
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 20, // High so global halt doesn't interfere
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 1,
    };
    let cb = CircuitBreaker::new(config);
    // Clean up env vars
    std::env::remove_var("CB_MARKET_BLACKLIST_THRESHOLD");
    std::env::remove_var("CB_MARKET_BLACKLIST_SECS");
    cb
}

/// Test that a market is blacklisted after N consecutive mismatches.
#[tokio::test]
async fn test_market_blacklisted_after_threshold() {
    let cb = create_circuit_breaker_with_blacklist_threshold(3);

    // Not blacklisted initially
    assert!(!cb.is_market_blacklisted("market1").await);

    // First two mismatches — still allowed
    cb.record_mismatch("market1").await;
    cb.record_mismatch("market1").await;
    assert!(!cb.is_market_blacklisted("market1").await);

    // Third mismatch — blacklisted
    cb.record_mismatch("market1").await;
    assert!(cb.is_market_blacklisted("market1").await);
}

/// Test that blacklisting is per-market (other markets unaffected).
#[tokio::test]
async fn test_blacklist_is_per_market() {
    let cb = create_circuit_breaker_with_blacklist_threshold(2);

    cb.record_mismatch("market1").await;
    cb.record_mismatch("market1").await;

    assert!(cb.is_market_blacklisted("market1").await, "market1 should be blacklisted");
    assert!(!cb.is_market_blacklisted("market2").await, "market2 should NOT be blacklisted");
}

/// Test that a successful matched fill resets the mismatch counter.
#[tokio::test]
async fn test_success_clears_mismatch_counter() {
    let cb = create_circuit_breaker_with_blacklist_threshold(3);

    // 2 mismatches, then a success
    cb.record_mismatch("market1").await;
    cb.record_mismatch("market1").await;
    cb.clear_market_mismatches("market1").await;

    // After clear, next mismatch is #1 again — not blacklisted
    cb.record_mismatch("market1").await;
    assert!(!cb.is_market_blacklisted("market1").await);

    // Need 2 more to hit threshold again
    cb.record_mismatch("market1").await;
    assert!(!cb.is_market_blacklisted("market1").await);
    cb.record_mismatch("market1").await;
    assert!(cb.is_market_blacklisted("market1").await);
}

/// Test that blacklist auto-expires after the configured duration.
#[tokio::test]
async fn test_blacklist_expires() {
    let cb = create_circuit_breaker_with_blacklist_threshold(2);

    // Blacklist market1
    cb.record_mismatch("market1").await;
    cb.record_mismatch("market1").await;
    assert!(cb.is_market_blacklisted("market1").await);

    // Wait for expiry (1s configured + buffer)
    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

    // Should be unblacklisted now
    assert!(!cb.is_market_blacklisted("market1").await);
}

/// Test that mismatches count toward global consecutive errors.
#[tokio::test]
async fn test_mismatch_escalates_to_global_halt() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 50.0,
        max_total_position: 250.0,
        max_daily_loss: 100.0,
        max_consecutive_errors: 3,
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 1,
    };
    let cb = CircuitBreaker::new(config);

    assert!(cb.is_trading_allowed());

    // 3 mismatches (even across different markets) should trip global halt
    cb.record_mismatch("market1").await;
    cb.record_mismatch("market2").await;
    assert!(cb.is_trading_allowed(), "Should still be allowed after 2 mismatches");

    cb.record_mismatch("market3").await;
    assert!(!cb.is_trading_allowed(), "Should be halted after 3 mismatches (consecutive error limit)");
}

/// Test that auto-close P&L feeds back into daily loss tracking.
#[tokio::test]
async fn test_auto_close_pnl_feeds_daily_loss() {
    let cb = create_circuit_breaker_with_limits(50.0, 250.0, 10.0); // $10 daily loss limit

    // Simulate auto-close losses via record_pnl
    cb.record_pnl(-5.0);  // $5 loss
    assert!(cb.can_execute("market1", 0.5).await.is_ok(), "Should still be allowed");

    cb.record_pnl(-6.0);  // $6 more loss = $11 total > $10 limit

    let result = cb.can_execute("market1", 0.5).await;
    assert!(
        matches!(result, Err(TripReason::MaxDailyLoss { .. })),
        "Should be blocked by daily loss from auto-close P&L"
    );
}

/// Test that blacklisting is skipped when circuit breaker is disabled.
#[tokio::test]
async fn test_blacklist_disabled_when_cb_disabled() {
    let cb = create_disabled_circuit_breaker();

    // Record many mismatches
    for _ in 0..10 {
        cb.record_mismatch("market1").await;
    }

    // Should never be blacklisted when CB is disabled
    assert!(!cb.is_market_blacklisted("market1").await);
}

/// Test that clear_market_mismatches does not unblacklist an already-blacklisted market.
/// (Only expiry should clear a blacklist, not a success during the blacklist period.)
#[tokio::test]
async fn test_clear_mismatches_does_not_unblacklist() {
    let cb = create_circuit_breaker_with_blacklist_threshold(2);

    // Blacklist market1
    cb.record_mismatch("market1").await;
    cb.record_mismatch("market1").await;
    assert!(cb.is_market_blacklisted("market1").await);

    // clear_market_mismatches should NOT unblacklist (only expiry does)
    cb.clear_market_mismatches("market1").await;
    assert!(cb.is_market_blacklisted("market1").await, "Should still be blacklisted");
}
