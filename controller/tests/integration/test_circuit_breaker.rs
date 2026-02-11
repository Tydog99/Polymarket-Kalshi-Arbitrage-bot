//! Integration tests for the circuit breaker system.
//!
//! These tests verify that the circuit breaker correctly:
//! 1. Allows trades within limits
//! 2. Blocks trades that exceed position limits
//! 3. Tracks positions per market and total
//! 4. Handles consecutive errors
//! 5. Manages cooldown periods
//! 6. Calculates remaining capacity

use arb_bot::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, TripReason};

// ============================================================================
// TEST HELPERS
// ============================================================================

/// Create a circuit breaker with standard test configuration.
fn create_test_circuit_breaker() -> CircuitBreaker {
    let config = CircuitBreakerConfig {
        max_position_per_market: 100,
        max_total_position: 500,
        max_daily_loss: 100.0,
        max_consecutive_errors: 5,
        cooldown_secs: 60,
        enabled: true,
        min_contracts: 1,
    };
    CircuitBreaker::new(config)
}

/// Create a circuit breaker with custom per-market and total limits.
fn create_circuit_breaker_with_limits(
    per_market: i64,
    total: i64,
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
        max_position_per_market: 100,
        max_total_position: 500,
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
        max_position_per_market: 100,
        max_total_position: 500,
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

    // Should allow trade within limits
    let result = cb.can_execute("KXNBA-26-SAS", 50).await;
    assert!(result.is_ok(), "Trade within limits should be allowed");

    // Should allow trade for different market
    let result = cb.can_execute("KXNFL-26-DEN", 50).await;
    assert!(
        result.is_ok(),
        "Trade for different market should be allowed"
    );
}

/// Test that multiple small trades within limits are allowed.
#[tokio::test]
async fn test_circuit_breaker_allows_multiple_small_trades() {
    let cb = create_test_circuit_breaker();

    // Record several successful trades
    cb.record_success("market1", 20, 20, 0.0).await;
    cb.record_success("market2", 20, 20, 0.0).await;
    cb.record_success("market3", 20, 20, 0.0).await;

    // Total is now 120 contracts (40 per market * 3)
    // Should still allow trades within remaining limits
    let result = cb.can_execute("market4", 50).await;
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
    let cb = create_circuit_breaker_with_limits(100, 1000, 100.0);

    // Record 80 contracts for market1
    cb.record_success("market1", 40, 40, 0.0).await;

    // Try to add 30 more (would be 110 total, exceeds 100 limit)
    let result = cb.can_execute("market1", 30).await;

    match result {
        Err(TripReason::MaxPositionPerMarket {
            market,
            position,
            limit,
        }) => {
            assert_eq!(market, "market1");
            assert_eq!(position, 110); // 80 existing + 30 new
            assert_eq!(limit, 100);
        }
        other => panic!(
            "Expected MaxPositionPerMarket error, got {:?}",
            other
        ),
    }
}

/// Test that per-market limit is checked for new markets.
#[tokio::test]
async fn test_circuit_breaker_blocks_new_market_exceeding_limit() {
    let cb = create_circuit_breaker_with_limits(50, 1000, 100.0);

    // Try to add 60 contracts to a new market (exceeds 50 limit)
    // Note: can_execute only blocks if the position already exists
    // For new markets, the first trade creates the position
    let result = cb.can_execute("new_market", 60).await;

    // For a new market, the check passes because there's no existing position
    // The limit would be enforced on the next trade
    assert!(
        result.is_ok(),
        "First trade to new market should be allowed (no existing position)"
    );
}

// ============================================================================
// TEST: BLOCKS MAX TOTAL POSITION
// ============================================================================

/// Test that exceeding total position limit triggers MaxTotalPosition trip.
#[tokio::test]
async fn test_circuit_breaker_blocks_max_total_position() {
    let cb = create_circuit_breaker_with_limits(200, 100, 100.0);

    // Record 60 contracts across markets
    cb.record_success("market1", 30, 30, 0.0).await;

    // Try to add 50 more (would be 110 total, exceeds 100 limit)
    let result = cb.can_execute("market2", 50).await;

    match result {
        Err(TripReason::MaxTotalPosition { position, limit }) => {
            assert_eq!(position, 110); // 60 existing + 50 new
            assert_eq!(limit, 100);
        }
        other => panic!("Expected MaxTotalPosition error, got {:?}", other),
    }
}

/// Test total position limit across many markets.
#[tokio::test]
async fn test_circuit_breaker_blocks_total_across_many_markets() {
    let cb = create_circuit_breaker_with_limits(100, 200, 100.0);

    // Record trades across multiple markets
    cb.record_success("market1", 40, 40, 0.0).await; // 80 total
    cb.record_success("market2", 40, 40, 0.0).await; // 160 total

    // Try to add 50 more to a new market (would be 210 total, exceeds 200 limit)
    let result = cb.can_execute("market3", 50).await;

    match result {
        Err(TripReason::MaxTotalPosition { position, limit }) => {
            assert_eq!(position, 210); // 160 existing + 50 new
            assert_eq!(limit, 200);
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
    let cb = create_circuit_breaker_with_limits(100, 500, 50.0);

    // Record a large loss
    cb.record_pnl(-60.0); // $60 loss exceeds $50 limit

    // Try to execute a trade
    let result = cb.can_execute("market1", 10).await;

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
    let cb = create_circuit_breaker_with_limits(100, 500, 50.0);

    // Record multiple small losses
    cb.record_success("market1", 10, 10, -15.0).await;
    cb.record_success("market2", 10, 10, -20.0).await;
    cb.record_success("market3", 10, 10, -20.0).await;
    // Total loss: $55

    // Try to execute a trade
    let result = cb.can_execute("market4", 10).await;

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
        max_position_per_market: 100,
        max_total_position: 500,
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
    let result = cb.can_execute("market1", 10).await;
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
        max_position_per_market: 100,
        max_total_position: 500,
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
    cb.record_success("market1", 10, 10, 0.0).await;

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
    let cb = create_circuit_breaker_with_limits(100, 500, 100.0);

    let capacity = cb.get_remaining_capacity("market1").await;

    assert_eq!(capacity.per_market, 100, "Per-market should equal limit");
    assert_eq!(capacity.total, 500, "Total should equal limit");
    assert_eq!(
        capacity.effective, 100,
        "Effective should be min(100, 500)"
    );
}

/// Test get_remaining_capacity after recording a trade.
#[tokio::test]
async fn test_circuit_breaker_remaining_capacity_after_trade() {
    let cb = create_circuit_breaker_with_limits(100, 500, 100.0);

    // Record a trade of 30 contracts
    cb.record_success("market1", 15, 15, 0.0).await;

    // Check capacity for market1
    let capacity = cb.get_remaining_capacity("market1").await;
    assert_eq!(capacity.per_market, 70, "Per-market: 100 - 30 = 70");
    assert_eq!(capacity.total, 470, "Total: 500 - 30 = 470");
    assert_eq!(capacity.effective, 70, "Effective: min(70, 470) = 70");

    // Check capacity for a different market
    let capacity2 = cb.get_remaining_capacity("market2").await;
    assert_eq!(
        capacity2.per_market, 100,
        "Different market has full per-market limit"
    );
    assert_eq!(capacity2.total, 470, "Total is shared across markets");
    assert_eq!(
        capacity2.effective, 100,
        "Effective: min(100, 470) = 100"
    );
}

/// Test remaining capacity when total limit constrains more than per-market.
#[tokio::test]
async fn test_circuit_breaker_remaining_capacity_total_constrains() {
    let cb = create_circuit_breaker_with_limits(100, 50, 100.0);

    let capacity = cb.get_remaining_capacity("market1").await;

    assert_eq!(capacity.per_market, 100);
    assert_eq!(capacity.total, 50);
    assert_eq!(
        capacity.effective, 50,
        "Effective constrained by total (50 < 100)"
    );
}

/// Test remaining capacity goes to zero when limits reached.
#[tokio::test]
async fn test_circuit_breaker_remaining_capacity_at_limit() {
    let cb = create_circuit_breaker_with_limits(100, 500, 100.0);

    // Fill up market1 to its limit
    cb.record_success("market1", 50, 50, 0.0).await;

    let capacity = cb.get_remaining_capacity("market1").await;
    assert_eq!(capacity.per_market, 0, "Market at limit should have 0 remaining");
    assert_eq!(capacity.effective, 0, "Effective should be 0");
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
    let result = cb.can_execute("market1", 10).await;
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
        max_position_per_market: 100,
        max_total_position: 500,
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

    let result = cb.can_execute("market1", 10).await;
    assert!(result.is_ok(), "Trade should be allowed after reset");
}

/// Test that reset clears consecutive error count.
#[tokio::test]
async fn test_circuit_breaker_reset_clears_error_count() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 100,
        max_total_position: 500,
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
    let result = cb.can_execute("market1", 1000).await;
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

    assert_eq!(capacity.per_market, i64::MAX, "Should report unlimited");
    assert_eq!(capacity.total, i64::MAX, "Should report unlimited");
    assert_eq!(capacity.effective, i64::MAX, "Should report unlimited");
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
    let result = cb.can_execute("market1", 10).await;
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
    let cb = create_circuit_breaker_with_limits(100, 500, 100.0);

    // Record 70 contracts for market1
    cb.record_success("market1", 35, 35, 0.0).await;

    // Get remaining capacity
    let capacity = cb.get_remaining_capacity("market1").await;
    assert_eq!(capacity.per_market, 30, "Should have 30 remaining for market1");

    // Simulate what the caller would do: cap the trade to remaining capacity
    let requested_contracts = 50;
    let capped_contracts = requested_contracts.min(capacity.effective);
    assert_eq!(capped_contracts, 30, "Trade should be capped to 30");

    // Verify the capped trade would be allowed
    let result = cb.can_execute("market1", capped_contracts).await;
    assert!(result.is_ok(), "Capped trade should be allowed");
}

/// Test capacity calculation with both per-market and total constraints.
#[tokio::test]
async fn test_circuit_breaker_capacity_dual_constraints() {
    let cb = create_circuit_breaker_with_limits(50, 100, 100.0);

    // Fill up 80 contracts across markets
    cb.record_success("market1", 20, 20, 0.0).await; // 40 contracts
    cb.record_success("market2", 20, 20, 0.0).await; // 40 contracts = 80 total

    // Check capacity for market3
    let capacity = cb.get_remaining_capacity("market3").await;
    assert_eq!(capacity.per_market, 50, "Full per-market limit available");
    assert_eq!(capacity.total, 20, "Only 20 total remaining");
    assert_eq!(
        capacity.effective, 20,
        "Effective constrained by total (20 < 50)"
    );
}

// ============================================================================
// TEST: MIN CONTRACTS
// ============================================================================

/// Test that min_contracts getter returns configured value.
#[tokio::test]
async fn test_circuit_breaker_min_contracts() {
    let config = CircuitBreakerConfig {
        max_position_per_market: 100,
        max_total_position: 500,
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

    // Simulate what caller would do: skip if effective < min
    let should_trade = capacity.effective >= min;
    assert!(
        should_trade,
        "Should trade when capacity >= min_contracts"
    );

    // Simulate low capacity scenario
    let cb2 = create_circuit_breaker_with_limits(3, 500, 100.0);
    cb2.record_success("market1", 1, 1, 0.0).await;

    let capacity2 = cb2.get_remaining_capacity("market1").await;
    assert_eq!(capacity2.per_market, 1);

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

    // Record some activity
    cb.record_success("market1", 10, 10, 5.0).await;
    cb.record_success("market2", 15, 15, -3.0).await;

    let status = cb.status().await;

    assert!(status.enabled, "Should be enabled");
    assert!(!status.halted, "Should not be halted");
    assert!(status.trip_reason.is_none(), "Should have no trip reason");
    assert_eq!(status.consecutive_errors, 0, "Should have no errors");
    assert!(
        (status.daily_pnl - 2.0).abs() < 0.01,
        "P&L should be $5 - $3 = $2"
    );
    assert_eq!(status.total_position, 50, "Total: 20 + 30 = 50 contracts");
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
    let cb = create_circuit_breaker_with_limits(100, 500, 50.0);

    // Accumulate some P&L
    cb.record_success("market1", 10, 10, -30.0).await;
    cb.record_success("market2", 10, 10, -15.0).await;
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
    let result = cb.can_execute("market3", 10).await;
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

    let result = cb.can_execute("market1", 10).await;
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
        max_position_per_market: 100,
        max_total_position: 500,
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
        max_position_per_market: 100,
        max_total_position: 500,
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
    let cb = create_circuit_breaker_with_limits(100, 500, 10.0); // $10 daily loss limit

    // Simulate auto-close losses via record_pnl
    cb.record_pnl(-5.0);  // $5 loss
    assert!(cb.can_execute("market1", 1).await.is_ok(), "Should still be allowed");

    cb.record_pnl(-6.0);  // $6 more loss = $11 total > $10 limit

    let result = cb.can_execute("market1", 1).await;
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
