//! Integration tests for the position tracker system.
//!
//! These tests verify that the position tracker correctly:
//! 1. Records fills and updates positions
//! 2. Calculates matched/unmatched contracts
//! 3. Calculates guaranteed profit for arb positions
//! 4. Persists and loads state from JSON

use arb_bot::position_tracker::{FillRecord, PositionTracker};
use std::fs;
use tempfile::NamedTempFile;

// ============================================================================
// TEST HELPERS
// ============================================================================

/// Create a FillRecord for testing.
fn create_fill(
    market_id: &str,
    platform: &str,
    side: &str,
    contracts: f64,
    price: f64,
    fees: f64,
) -> FillRecord {
    FillRecord::new(
        market_id,
        "Test Market",
        platform,
        side,
        contracts,
        price,
        fees,
        "test-order-123",
    )
}

// ============================================================================
// TEST: RECORD KALSHI FILL
// ============================================================================

/// Test that recording a Kalshi fill correctly updates the position.
#[test]
fn test_position_tracker_record_kalshi_fill() {
    let mut tracker = PositionTracker::new();

    let fill = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        10.0,  // contracts
        0.45,  // price ($0.45)
        0.05,  // fees
    );

    tracker.record_fill_internal(&fill);

    let pos = tracker.get("KXNBA-26-SAS").expect("Position should exist");

    // Verify the Kalshi NO leg was updated
    assert_eq!(pos.kalshi_no.contracts, 10.0);
    assert!((pos.kalshi_no.cost_basis - 4.5).abs() < 0.001, "Cost basis should be 10 * 0.45 = 4.50");
    assert!((pos.kalshi_no.avg_price - 0.45).abs() < 0.001, "Avg price should be 0.45");

    // Other legs should be empty
    assert_eq!(pos.kalshi_yes.contracts, 0.0);
    assert_eq!(pos.poly_yes.contracts, 0.0);
    assert_eq!(pos.poly_no.contracts, 0.0);

    // Fees should be recorded
    assert!((pos.total_fees - 0.05).abs() < 0.001, "Fees should be 0.05");
}

// ============================================================================
// TEST: RECORD POLYMARKET FILL
// ============================================================================

/// Test that recording a Polymarket fill correctly updates the position.
#[test]
fn test_position_tracker_record_poly_fill() {
    let mut tracker = PositionTracker::new();

    let fill = create_fill(
        "KXNBA-26-SAS",
        "polymarket",
        "yes",
        15.0,  // contracts
        0.52,  // price ($0.52)
        0.0,   // fees (Polymarket has no fees)
    );

    tracker.record_fill_internal(&fill);

    let pos = tracker.get("KXNBA-26-SAS").expect("Position should exist");

    // Verify the Polymarket YES leg was updated
    assert_eq!(pos.poly_yes.contracts, 15.0);
    assert!((pos.poly_yes.cost_basis - 7.8).abs() < 0.001, "Cost basis should be 15 * 0.52 = 7.80");
    assert!((pos.poly_yes.avg_price - 0.52).abs() < 0.001, "Avg price should be 0.52");

    // Other legs should be empty
    assert_eq!(pos.kalshi_yes.contracts, 0.0);
    assert_eq!(pos.kalshi_no.contracts, 0.0);
    assert_eq!(pos.poly_no.contracts, 0.0);

    // No fees for Polymarket
    assert_eq!(pos.total_fees, 0.0);
}

// ============================================================================
// TEST: MATCHED ARB
// ============================================================================

/// Test a matched arb position (Kalshi NO + Polymarket YES).
///
/// In prediction markets, YES + NO = $1.00. When we buy YES on one platform
/// and NO on another for less than $1.00 total, we have a guaranteed profit.
#[test]
fn test_position_tracker_matched_arb() {
    let mut tracker = PositionTracker::new();

    // Buy 10 YES on Polymarket at 45 cents
    let poly_fill = create_fill(
        "KXNBA-26-SAS",
        "polymarket",
        "yes",
        10.0,
        0.45,
        0.0,
    );
    tracker.record_fill_internal(&poly_fill);

    // Buy 10 NO on Kalshi at 50 cents
    let kalshi_fill = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        10.0,
        0.50,
        0.05, // Kalshi fees
    );
    tracker.record_fill_internal(&kalshi_fill);

    let pos = tracker.get("KXNBA-26-SAS").expect("Position should exist");

    // Verify matched contracts
    // YES total: 10 (from Poly)
    // NO total: 10 (from Kalshi)
    // Matched: min(10, 10) = 10
    assert!((pos.matched_contracts() - 10.0).abs() < 0.001, "Should have 10 matched contracts");

    // Verify total cost
    // Poly YES: 10 * 0.45 = $4.50
    // Kalshi NO: 10 * 0.50 = $5.00
    // Fees: $0.05
    // Total: $9.55
    assert!((pos.total_cost() - 9.55).abs() < 0.001, "Total cost should be $9.55");

    // Verify guaranteed profit
    // We hold 10 matched pairs, one side always wins, so we get $10.00
    // Profit = $10.00 - $9.55 = $0.45
    assert!((pos.guaranteed_profit() - 0.45).abs() < 0.001, "Guaranteed profit should be $0.45");

    // Verify no unmatched exposure (perfectly balanced)
    assert!((pos.unmatched_exposure() - 0.0).abs() < 0.001, "Should have 0 unmatched exposure");
}

// ============================================================================
// TEST: UNMATCHED EXPOSURE (ONE-SIDED FILL)
// ============================================================================

/// Test unmatched exposure when only one side of an arb is filled.
#[test]
fn test_position_tracker_unmatched_exposure() {
    let mut tracker = PositionTracker::new();

    // Only buy YES on Polymarket (no offsetting NO position)
    let fill = create_fill(
        "KXNBA-26-SAS",
        "polymarket",
        "yes",
        10.0,
        0.45,
        0.0,
    );
    tracker.record_fill_internal(&fill);

    let pos = tracker.get("KXNBA-26-SAS").expect("Position should exist");

    // Verify we have 10 YES contracts and 0 NO contracts
    assert_eq!(pos.poly_yes.contracts, 10.0);
    assert_eq!(pos.kalshi_no.contracts, 0.0);
    assert_eq!(pos.kalshi_yes.contracts, 0.0);
    assert_eq!(pos.poly_no.contracts, 0.0);

    // Matched contracts should be 0 (min of YES=10 and NO=0)
    assert!((pos.matched_contracts() - 0.0).abs() < 0.001, "Should have 0 matched contracts");

    // Unmatched exposure should be 10 (all on one side)
    assert!((pos.unmatched_exposure() - 10.0).abs() < 0.001, "Should have 10 unmatched contracts");

    // Guaranteed profit is negative (we're exposed)
    // Profit = matched(0) - cost(4.50) = -4.50
    assert!((pos.guaranteed_profit() - (-4.5)).abs() < 0.001, "Guaranteed profit should be -$4.50");
}

// ============================================================================
// TEST: PARTIAL FILL IMBALANCE
// ============================================================================

/// Test imbalance calculation when fills are different sizes.
///
/// This simulates a partial fill scenario where one leg fills more than the other.
#[test]
fn test_position_tracker_partial_fill_imbalance() {
    let mut tracker = PositionTracker::new();

    // Buy 10 YES on Polymarket at 45 cents
    let poly_fill = create_fill(
        "KXNBA-26-SAS",
        "polymarket",
        "yes",
        10.0,
        0.45,
        0.0,
    );
    tracker.record_fill_internal(&poly_fill);

    // Only get 8 NO filled on Kalshi at 50 cents (partial fill)
    let kalshi_fill = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        8.0,
        0.50,
        0.04, // Slightly lower fees for fewer contracts
    );
    tracker.record_fill_internal(&kalshi_fill);

    let pos = tracker.get("KXNBA-26-SAS").expect("Position should exist");

    // Verify matched contracts = min(YES=10, NO=8) = 8
    assert!((pos.matched_contracts() - 8.0).abs() < 0.001, "Should have 8 matched contracts");

    // Verify unmatched exposure = |YES - NO| = |10 - 8| = 2
    assert!((pos.unmatched_exposure() - 2.0).abs() < 0.001, "Should have 2 unmatched contracts");

    // Verify total cost
    // Poly YES: 10 * 0.45 = $4.50
    // Kalshi NO: 8 * 0.50 = $4.00
    // Fees: $0.04
    // Total: $8.54
    assert!((pos.total_cost() - 8.54).abs() < 0.001, "Total cost should be $8.54");

    // Verify guaranteed profit (only from matched pairs)
    // Matched: 8 contracts = $8.00 payout
    // Total cost: $8.54
    // Profit: $8.00 - $8.54 = -$0.54
    assert!((pos.guaranteed_profit() - (-0.54)).abs() < 0.001, "Guaranteed profit should be -$0.54");
}

// ============================================================================
// TEST: MULTIPLE FILLS ON SAME LEG
// ============================================================================

/// Test that multiple fills on the same leg are accumulated correctly.
#[test]
fn test_position_tracker_multiple_fills_same_leg() {
    let mut tracker = PositionTracker::new();

    // First fill: 5 contracts at 40 cents
    let fill1 = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        5.0,
        0.40,
        0.02,
    );
    tracker.record_fill_internal(&fill1);

    // Second fill: 10 contracts at 50 cents
    let fill2 = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        10.0,
        0.50,
        0.04,
    );
    tracker.record_fill_internal(&fill2);

    let pos = tracker.get("KXNBA-26-SAS").expect("Position should exist");

    // Verify accumulated contracts
    assert!((pos.kalshi_no.contracts - 15.0).abs() < 0.001, "Should have 15 contracts");

    // Verify accumulated cost basis
    // First: 5 * 0.40 = $2.00
    // Second: 10 * 0.50 = $5.00
    // Total cost basis: $7.00
    assert!((pos.kalshi_no.cost_basis - 7.0).abs() < 0.001, "Cost basis should be $7.00");

    // Verify average price
    // Avg = cost_basis / contracts = 7.00 / 15 = 0.4667
    assert!((pos.kalshi_no.avg_price - (7.0 / 15.0)).abs() < 0.001, "Avg price should be ~$0.467");

    // Verify accumulated fees
    assert!((pos.total_fees - 0.06).abs() < 0.001, "Total fees should be $0.06");
}

// ============================================================================
// TEST: PERSISTENCE (SAVE AND LOAD)
// ============================================================================

/// Test that position state persists correctly to JSON and can be reloaded.
#[test]
fn test_position_tracker_persistence() {
    // Create a temporary file for the test
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    let path = temp_file.path();

    // Create tracker and add some positions
    let mut tracker = PositionTracker::new();

    // Add a matched arb position
    let poly_fill = create_fill(
        "KXNBA-26-SAS",
        "polymarket",
        "yes",
        20.0,
        0.48,
        0.0,
    );
    tracker.record_fill_internal(&poly_fill);

    let kalshi_fill = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        20.0,
        0.47,
        0.10,
    );
    tracker.record_fill_internal(&kalshi_fill);

    // Add another position (unmatched)
    let poly_fill2 = create_fill(
        "KXNFL-26-KC",
        "polymarket",
        "yes",
        5.0,
        0.65,
        0.0,
    );
    tracker.record_fill_internal(&poly_fill2);

    // Save to temp file
    tracker.save_to(path).expect("Failed to save tracker");

    // Verify the file was created and contains JSON
    let contents = fs::read_to_string(path).expect("Failed to read saved file");
    assert!(contents.contains("KXNBA-26-SAS"), "Should contain first market ID");
    assert!(contents.contains("KXNFL-26-KC"), "Should contain second market ID");
    assert!(contents.contains("positions"), "Should contain positions field");

    // Load from temp file
    let loaded_tracker = PositionTracker::load_from(path);

    // Verify loaded state matches original
    let loaded_pos1 = loaded_tracker.get("KXNBA-26-SAS").expect("First position should exist");
    assert!((loaded_pos1.poly_yes.contracts - 20.0).abs() < 0.001, "Poly YES contracts should match");
    assert!((loaded_pos1.kalshi_no.contracts - 20.0).abs() < 0.001, "Kalshi NO contracts should match");
    assert!((loaded_pos1.matched_contracts() - 20.0).abs() < 0.001, "Matched contracts should match");

    let loaded_pos2 = loaded_tracker.get("KXNFL-26-KC").expect("Second position should exist");
    assert!((loaded_pos2.poly_yes.contracts - 5.0).abs() < 0.001, "Second position contracts should match");
    assert!((loaded_pos2.unmatched_exposure() - 5.0).abs() < 0.001, "Unmatched exposure should match");
}

// ============================================================================
// TEST: LOAD FROM NON-EXISTENT FILE
// ============================================================================

/// Test that loading from a non-existent file creates a fresh tracker.
#[test]
fn test_position_tracker_load_nonexistent() {
    let tracker = PositionTracker::load_from("/tmp/nonexistent_position_file_12345.json");

    // Should create an empty tracker
    let summary = tracker.summary();
    assert_eq!(summary.open_positions, 0, "Should have no positions");
    assert_eq!(summary.total_contracts, 0.0, "Should have no contracts");
}

// ============================================================================
// TEST: POSITION RESOLUTION
// ============================================================================

/// Test that resolving a position calculates P&L correctly.
#[test]
fn test_position_tracker_resolution_yes_wins() {
    let mut tracker = PositionTracker::new();

    // Buy 10 YES on Polymarket at 45 cents
    let poly_fill = create_fill(
        "KXNBA-26-SAS",
        "polymarket",
        "yes",
        10.0,
        0.45,
        0.0,
    );
    tracker.record_fill_internal(&poly_fill);

    // Buy 10 NO on Kalshi at 50 cents
    let kalshi_fill = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        10.0,
        0.50,
        0.05,
    );
    tracker.record_fill_internal(&kalshi_fill);

    // Resolve with YES winning
    let pnl = tracker.resolve_position("KXNBA-26-SAS", true);

    // YES wins: Polymarket YES pays out $10.00
    // Total cost: $4.50 + $5.00 + $0.05 = $9.55
    // P&L: $10.00 - $9.55 = $0.45
    assert!(pnl.is_some(), "Should return P&L");
    assert!((pnl.unwrap() - 0.45).abs() < 0.001, "P&L should be $0.45");

    // Verify position is marked resolved
    let pos = tracker.get("KXNBA-26-SAS").expect("Position should exist");
    assert_eq!(pos.status, "resolved");
    assert!(pos.realized_pnl.is_some());
}

/// Test that resolving a position calculates P&L correctly when NO wins.
#[test]
fn test_position_tracker_resolution_no_wins() {
    let mut tracker = PositionTracker::new();

    // Buy 10 YES on Polymarket at 45 cents
    let poly_fill = create_fill(
        "KXNBA-26-SAS",
        "polymarket",
        "yes",
        10.0,
        0.45,
        0.0,
    );
    tracker.record_fill_internal(&poly_fill);

    // Buy 10 NO on Kalshi at 50 cents
    let kalshi_fill = create_fill(
        "KXNBA-26-SAS",
        "kalshi",
        "no",
        10.0,
        0.50,
        0.05,
    );
    tracker.record_fill_internal(&kalshi_fill);

    // Resolve with NO winning
    let pnl = tracker.resolve_position("KXNBA-26-SAS", false);

    // NO wins: Kalshi NO pays out $10.00
    // Total cost: $4.50 + $5.00 + $0.05 = $9.55
    // P&L: $10.00 - $9.55 = $0.45
    // Same profit regardless of outcome (that's the arbitrage!)
    assert!(pnl.is_some(), "Should return P&L");
    assert!((pnl.unwrap() - 0.45).abs() < 0.001, "P&L should be $0.45");
}

// ============================================================================
// TEST: SUMMARY STATISTICS
// ============================================================================

/// Test that summary statistics are calculated correctly.
#[test]
fn test_position_tracker_summary() {
    let mut tracker = PositionTracker::new();

    // Add first position (matched arb)
    tracker.record_fill_internal(&create_fill("KXNBA-26-SAS", "polymarket", "yes", 10.0, 0.45, 0.0));
    tracker.record_fill_internal(&create_fill("KXNBA-26-SAS", "kalshi", "no", 10.0, 0.50, 0.05));

    // Add second position (partially matched)
    tracker.record_fill_internal(&create_fill("KXNFL-26-KC", "polymarket", "yes", 8.0, 0.60, 0.0));
    tracker.record_fill_internal(&create_fill("KXNFL-26-KC", "kalshi", "no", 5.0, 0.35, 0.02));

    let summary = tracker.summary();

    // Verify open positions count
    assert_eq!(summary.open_positions, 2, "Should have 2 open positions");

    // Verify total contracts
    // Position 1: 10 YES + 10 NO = 20
    // Position 2: 8 YES + 5 NO = 13
    // Total: 33
    assert!((summary.total_contracts - 33.0).abs() < 0.001, "Should have 33 total contracts");

    // Verify total cost basis
    // Position 1: 4.50 + 5.00 + 0.05 = 9.55
    // Position 2: 4.80 + 1.75 + 0.02 = 6.57
    // Total: 16.12
    assert!((summary.total_cost_basis - 16.12).abs() < 0.01, "Total cost basis should be ~$16.12");

    // Verify unmatched exposure
    // Position 1: |10 - 10| = 0
    // Position 2: |8 - 5| = 3
    // Total: 3
    assert!((summary.total_unmatched_exposure - 3.0).abs() < 0.001, "Unmatched exposure should be 3");
}

// ============================================================================
// TEST: ALL FOUR LEG TYPES
// ============================================================================

/// Test that all four leg types (kalshi_yes, kalshi_no, poly_yes, poly_no) work.
#[test]
fn test_position_tracker_all_leg_types() {
    let mut tracker = PositionTracker::new();
    let market = "KXTEST-MARKET";

    // Record a fill on each leg type
    tracker.record_fill_internal(&create_fill(market, "kalshi", "yes", 1.0, 0.25, 0.01));
    tracker.record_fill_internal(&create_fill(market, "kalshi", "no", 2.0, 0.30, 0.02));
    tracker.record_fill_internal(&create_fill(market, "polymarket", "yes", 3.0, 0.35, 0.0));
    tracker.record_fill_internal(&create_fill(market, "polymarket", "no", 4.0, 0.40, 0.0));

    let pos = tracker.get(market).expect("Position should exist");

    // Verify each leg
    assert!((pos.kalshi_yes.contracts - 1.0).abs() < 0.001, "Kalshi YES should have 1 contract");
    assert!((pos.kalshi_no.contracts - 2.0).abs() < 0.001, "Kalshi NO should have 2 contracts");
    assert!((pos.poly_yes.contracts - 3.0).abs() < 0.001, "Poly YES should have 3 contracts");
    assert!((pos.poly_no.contracts - 4.0).abs() < 0.001, "Poly NO should have 4 contracts");

    // Total contracts: 1 + 2 + 3 + 4 = 10
    assert!((pos.total_contracts() - 10.0).abs() < 0.001, "Should have 10 total contracts");

    // YES total: 1 + 3 = 4
    // NO total: 2 + 4 = 6
    // Matched: min(4, 6) = 4
    assert!((pos.matched_contracts() - 4.0).abs() < 0.001, "Should have 4 matched contracts");

    // Unmatched: |4 - 6| = 2
    assert!((pos.unmatched_exposure() - 2.0).abs() < 0.001, "Should have 2 unmatched contracts");
}
