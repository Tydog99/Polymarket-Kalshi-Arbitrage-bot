// tests/integration_tests.rs
// Holistic integration tests for the arbitrage bot
//
// These tests verify the full flow:
// 1. Arb detection (with fee awareness)
// 2. Position tracking after fills
// 3. Circuit breaker behavior
// 4. End-to-end scenarios

// Note: Arc and RwLock are used by various test modules below

// Note: Old helper function `make_market_state` and `detection_tests` module were removed
// as they tested the old non-atomic MarketArbState architecture that has been deleted.
// The atomic-equivalent tests are in the `integration_tests` module below.

// ============================================================================
// POSITION TRACKER TESTS - Verify fill recording and P&L calculation
// ============================================================================

mod position_tracker_tests {
    use arb_bot::position_tracker::*;
    
    /// Test: Recording fills updates position correctly
    #[test]
    fn test_record_fills_updates_position() {
        let mut tracker = PositionTracker::new();
        
        // Record a Kalshi NO fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Market",
            "kalshi",
            "no",
            10.0,   // 10 contracts
            0.50,   // at 50¢
            0.18,   // 18¢ fees (for 10 contracts)
            "order123",
        ));
        
        // Record a Polymarket YES fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Market",
            "polymarket",
            "yes",
            10.0,   // 10 contracts
            0.40,   // at 40¢
            0.0,    // no fees
            "order456",
        ));
        
        let summary = tracker.summary();
        
        assert_eq!(summary.open_positions, 1, "Should have 1 open position");
        assert!(summary.total_contracts > 0.0, "Should have contracts");
        
        // Cost basis: 10 * 0.50 + 10 * 0.40 = $9.00 + fees
        assert!(summary.total_cost_basis > 9.0, "Cost basis should be > $9");
    }
    
    /// Test: Matched arb calculates guaranteed profit
    #[test]
    fn test_matched_arb_guaranteed_profit() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        // Buy 10 YES on Poly at 40¢
        pos.poly_yes.add(10.0, 0.40);
        
        // Buy 10 NO on Kalshi at 50¢
        pos.kalshi_no.add(10.0, 0.50);
        
        // Add fees
        pos.total_fees = 0.18;  // 18¢ fees
        
        // Total cost: $4.00 + $5.00 + $0.18 = $9.18
        // Guaranteed payout: $10.00
        // Guaranteed profit: $0.82
        
        assert!((pos.total_cost() - 9.18).abs() < 0.01, "Cost should be $9.18");
        assert!((pos.matched_contracts() - 10.0).abs() < 0.01, "Should have 10 matched");
        assert!(pos.guaranteed_profit() > 0.0, "Should have positive guaranteed profit");
        assert!((pos.guaranteed_profit() - 0.82).abs() < 0.01, "Profit should be ~$0.82");
    }
    
    /// Test: Partial fills create exposure
    #[test]
    fn test_partial_fill_creates_exposure() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        // Full fill on Poly YES
        pos.poly_yes.add(10.0, 0.40);
        
        // Partial fill on Kalshi NO (only 7 contracts)
        pos.kalshi_no.add(7.0, 0.50);
        
        assert!((pos.matched_contracts() - 7.0).abs() < 0.01, "Should have 7 matched");
        assert!((pos.unmatched_exposure() - 3.0).abs() < 0.01, "Should have 3 unmatched");
    }
    
    /// Test: Position resolution calculates realized P&L
    #[test]
    fn test_position_resolution() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        pos.poly_yes.add(10.0, 0.40);   // Cost: $4.00
        pos.kalshi_no.add(10.0, 0.50);  // Cost: $5.00
        pos.total_fees = 0.18;           // Fees: $0.18
        
        // YES wins → Poly YES pays $10
        pos.resolve(true);
        
        assert_eq!(pos.status, "resolved");
        let pnl = pos.realized_pnl.expect("Should have realized P&L");
        
        // Payout: $10 - Cost: $9.18 = $0.82 profit
        assert!((pnl - 0.82).abs() < 0.01, "P&L should be ~$0.82, got {}", pnl);
    }
    
}

// ============================================================================
// CIRCUIT BREAKER TESTS - Verify safety limits
// ============================================================================

mod circuit_breaker_tests {
    use arb_bot::circuit_breaker::*;
    
    fn test_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            max_position_per_market: 50.0,
            max_total_position: 200.0,
            max_daily_loss: 25.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        }
    }
    
    /// Test: Allows trades within limits
    #[tokio::test]
    async fn test_allows_trades_within_limits() {
        let cb = CircuitBreaker::new(test_config());
        
        // First trade should be allowed
        let result = cb.can_execute("market1", 10.0).await;
        assert!(result.is_ok(), "Should allow first trade");

        // Record success (10 contracts at 50c each leg)
        cb.record_success("market1", 10, 50, 10, 50, 0.50).await;

        // Second trade on same market should still be allowed
        let result = cb.can_execute("market1", 10.0).await;
        assert!(result.is_ok(), "Should allow second trade within limit");
    }
    
    /// Test: Blocks trade exceeding per-market limit
    #[tokio::test]
    async fn test_blocks_per_market_limit() {
        let cb = CircuitBreaker::new(test_config());
        
        // Fill up the market (45 contracts at 50c each leg = $45 cost basis)
        cb.record_success("market1", 45, 50, 45, 50, 1.0).await;

        // Try to add $10 more (would exceed $50 limit)
        let result = cb.can_execute("market1", 10.0).await;

        assert!(matches!(result, Err(TripReason::MaxPositionPerMarket { .. })),
            "Should block trade exceeding per-market limit");
    }
    
    /// Test: Blocks trade exceeding total position limit
    #[tokio::test]
    async fn test_blocks_total_position_limit() {
        let cb = CircuitBreaker::new(test_config());
        
        // Fill up multiple markets (each at 50c per leg)
        cb.record_success("market1", 50, 50, 50, 50, 1.0).await;  // $50
        cb.record_success("market2", 50, 50, 50, 50, 1.0).await;  // $50
        cb.record_success("market3", 50, 50, 50, 50, 1.0).await;  // $50
        cb.record_success("market4", 45, 50, 45, 50, 1.0).await;  // $45, Total: $195

        // Try to add $10 more (would exceed $200 total limit)
        let result = cb.can_execute("market5", 10.0).await;
        
        assert!(matches!(result, Err(TripReason::MaxTotalPosition { .. })),
            "Should block trade exceeding total position limit");
    }
    
    /// Test: Consecutive errors trip the breaker
    #[tokio::test]
    async fn test_consecutive_errors_trip() {
        let cb = CircuitBreaker::new(test_config());
        
        // Record errors up to limit
        cb.record_error().await;
        assert!(cb.is_trading_allowed(), "Should still allow after 1 error");
        
        cb.record_error().await;
        assert!(cb.is_trading_allowed(), "Should still allow after 2 errors");
        
        cb.record_error().await;
        assert!(!cb.is_trading_allowed(), "Should halt after 3 errors");
        
        // Verify trip reason
        let status = cb.status().await;
        assert!(status.halted);
        assert!(matches!(status.trip_reason, Some(TripReason::ConsecutiveErrors { .. })));
    }
    
    /// Test: Success resets error count
    #[tokio::test]
    async fn test_success_resets_errors() {
        let cb = CircuitBreaker::new(test_config());
        
        // Record 2 errors
        cb.record_error().await;
        cb.record_error().await;
        
        // Record success (10 contracts at 50c each leg)
        cb.record_success("market1", 10, 50, 10, 50, 0.50).await;

        // Error count should be reset
        let status = cb.status().await;
        assert_eq!(status.consecutive_errors, 0, "Success should reset error count");
        
        // Should need 3 more errors to trip
        cb.record_error().await;
        cb.record_error().await;
        assert!(cb.is_trading_allowed());
    }
    
    /// Test: Manual reset clears halt
    #[tokio::test]
    async fn test_manual_reset() {
        let cb = CircuitBreaker::new(test_config());
        
        // Trip the breaker
        cb.record_error().await;
        cb.record_error().await;
        cb.record_error().await;
        assert!(!cb.is_trading_allowed());
        
        // Reset
        cb.reset().await;
        assert!(cb.is_trading_allowed(), "Should allow trading after reset");
        
        let status = cb.status().await;
        assert!(!status.halted);
        assert!(status.trip_reason.is_none());
    }
    
    /// Test: Disabled circuit breaker allows everything
    #[tokio::test]
    async fn test_disabled_allows_all() {
        let mut config = test_config();
        config.enabled = false;
        let cb = CircuitBreaker::new(config);
        
        // Should allow even excessive trades
        let result = cb.can_execute("market1", 1000.0).await;
        assert!(result.is_ok(), "Disabled CB should allow all trades");
        
        // Errors shouldn't trip it
        cb.record_error().await;
        cb.record_error().await;
        cb.record_error().await;
        cb.record_error().await;
        assert!(cb.is_trading_allowed(), "Disabled CB should never halt");
    }
}

// ============================================================================
// END-TO-END SCENARIO TESTS - Full flow simulation
// ============================================================================

mod e2e_tests {
    use arb_bot::position_tracker::*;
    use arb_bot::circuit_breaker::*;

    // Note: test_full_arb_lifecycle was removed because it used the deleted
    // make_market_state helper and MarketArbState::check_arbs method.
    // See arb_detection_tests::test_complete_arb_flow for the equivalent.

    /// Scenario: Circuit breaker halts trading after losses
    #[tokio::test]
    async fn test_circuit_breaker_halts_on_losses() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 10.0,  // Low threshold for test
            max_consecutive_errors: 5,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // Simulate a series of losing trades
        // (In reality this would come from actual fill data)
        cb.record_success("market1", 10, 50, 10, 50, -3.0).await;  // -$3
        cb.record_success("market2", 10, 50, 10, 50, -4.0).await;  // -$7 cumulative

        // Should still be allowed
        assert!(cb.can_execute("market3", 10.0).await.is_ok());

        // One more loss pushes over the limit
        cb.record_success("market3", 10, 50, 10, 50, -5.0).await;  // -$12 cumulative

        // Now should be blocked due to max daily loss
        let result = cb.can_execute("market4", 10.0).await;
        assert!(matches!(result, Err(TripReason::MaxDailyLoss { .. })),
            "Should halt due to max daily loss");
    }
    
    /// Scenario: Partial fill creates exposure warning
    #[tokio::test]
    async fn test_partial_fill_exposure_tracking() {
        let mut tracker = PositionTracker::new();
        
        // Full fill on one side
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "order1",
        ));
        
        // Partial fill on other side (slippage/liquidity issue)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "kalshi",
            "no",
            7.0,  // Only got 7!
            0.50,
            0.13,
            "order2",
        ));
        
        let summary = tracker.summary();
        
        // Should show exposure
        assert!(
            summary.total_unmatched_exposure > 0.0,
            "Should show unmatched exposure: {}",
            summary.total_unmatched_exposure
        );
        
        // Matched should be limited to the smaller fill
        let position = tracker.get("TEST-MARKET").expect("Should have position");
        assert!((position.matched_contracts() - 7.0).abs() < 0.01);
        assert!((position.unmatched_exposure() - 3.0).abs() < 0.01);
    }

    // Note: test_fees_prevent_false_arb was removed because it used the deleted
    // make_market_state helper and MarketArbState::check_arbs method.
    // See arb_detection_tests::test_fees_eliminate_marginal_arb for the equivalent.
}

// ============================================================================
// FILL DATA ACCURACY TESTS - Verify actual vs expected prices
// ============================================================================

mod fill_accuracy_tests {
    use arb_bot::position_tracker::*;
    
    /// Test: Actual fill price different from expected
    #[test]
    fn test_fill_price_slippage() {
        let mut tracker = PositionTracker::new();
        
        // Expected: buy at 40¢, but actually filled at 42¢ (slippage)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "polymarket",
            "yes",
            10.0,
            0.42,  // Actual fill price (worse than expected 0.40)
            0.0,
            "order1",
        ));
        
        let pos = tracker.get("TEST-MARKET").expect("Should have position");
        
        // Should use actual price
        assert!((pos.poly_yes.avg_price - 0.42).abs() < 0.001);
        assert!((pos.poly_yes.cost_basis - 4.20).abs() < 0.01);
    }
    
    /// Test: Multiple fills at different prices calculates weighted average
    #[test]
    fn test_multiple_fills_weighted_average() {
        let mut pos = ArbPosition::new("TEST", "Test");
        
        // First fill: 5 contracts at 40¢
        pos.poly_yes.add(5.0, 0.40);
        
        // Second fill: 5 contracts at 44¢ (price moved)
        pos.poly_yes.add(5.0, 0.44);
        
        // Weighted average: (5*0.40 + 5*0.44) / 10 = 0.42
        assert!((pos.poly_yes.avg_price - 0.42).abs() < 0.001);
        assert!((pos.poly_yes.cost_basis - 4.20).abs() < 0.01);
        assert!((pos.poly_yes.contracts - 10.0).abs() < 0.01);
    }
    
    /// Test: Actual fees from API response
    #[test]
    fn test_actual_fees_recorded() {
        let mut tracker = PositionTracker::new();
        
        // Kalshi reports actual fees in response
        // Expected: 18¢ for 10 contracts at 50¢
        // Actual from API: 20¢ (maybe market-specific fee)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.20,  // Actual fees from API
            "order1",
        ));
        
        let pos = tracker.get("TEST-MARKET").expect("Should have position");
        assert!((pos.total_fees - 0.20).abs() < 0.001, "Should use actual fees");
    }
}

// ============================================================================
// INFRASTRUCTURE INTEGRATION TESTS
// ============================================================================

mod infra_integration_tests {
    use arb_bot::types::*;
    use arb_bot::arb::kalshi_fee;

    /// Helper to create market state with prices
    fn setup_market(
        kalshi_yes: PriceCents,
        kalshi_no: PriceCents,
        poly_yes: PriceCents,
        poly_no: PriceCents,
    ) -> (GlobalState, u16) {
        setup_market_with_neg_risk(kalshi_yes, kalshi_no, poly_yes, poly_no, false)
    }

    /// Helper to create market state with prices and explicit neg_risk flag
    fn setup_market_with_neg_risk(
        kalshi_yes: PriceCents,
        kalshi_no: PriceCents,
        poly_yes: PriceCents,
        poly_no: PriceCents,
        neg_risk: bool,
    ) -> (GlobalState, u16) {
        let state = GlobalState::default();

        let pair = MarketPair {
            pair_id: "arb-test-market".into(),
            league: "epl".into(),
            market_type: MarketType::Moneyline,
            description: "Test Market".into(),
            kalshi_event_ticker: "KXEPLGAME-25DEC27CFCARS".into(),
            kalshi_market_ticker: "KXEPLGAME-25DEC27CFCARS-CFC".into(),
            kalshi_event_slug: "premier-league-game".into(),
            poly_slug: "arb-test".into(),
            poly_yes_token: "arb_yes_token".into(),
            poly_no_token: "arb_no_token".into(),
            line_value: None,
            team_suffix: Some("CFC".into()),
            neg_risk,
        };

        let market_id = state.add_pair(pair).unwrap();

        // Set prices
        let market = state.get_by_id(market_id).unwrap();
        market.kalshi.store(kalshi_yes, kalshi_no, 1000, 1000);
        market.poly.store(poly_yes, poly_no, 1000, 1000);

        (state, market_id)
    }

    // =========================================================================
    // Arb Detection Tests (ArbOpportunity::detect)
    // =========================================================================

    /// Test: detects clear cross-platform arb (Poly YES + Kalshi NO)
    #[test]
    fn test_detects_poly_yes_kalshi_no_arb() {
        // Poly YES 40¢ + Kalshi NO 50¢ = 90¢ raw
        // Kalshi fee on 50¢ = 2¢
        // Effective = 92¢ → ARB!
        let (state, market_id) = setup_market(55, 50, 40, 65);

        let market = state.get_by_id(market_id).unwrap();
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        let arb = arb.expect("Should detect arb");
        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo, "Should detect Poly YES + Kalshi NO arb");
    }

    /// Test: arb detection works with neg_risk=true markets
    /// This exercises the neg_risk code path to ensure the field is properly propagated
    #[test]
    fn test_detects_arb_with_neg_risk_true() {
        // Same as poly_yes_kalshi_no test but with neg_risk=true
        let (state, market_id) = setup_market_with_neg_risk(55, 50, 40, 65, true);

        let market = state.get_by_id(market_id).unwrap();

        // Verify neg_risk was stored correctly via the pair() accessor
        let pair = market.pair().expect("Should have pair");
        assert!(pair.neg_risk, "neg_risk should be true");

        // Arb detection should work the same regardless of neg_risk
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        let arb = arb.expect("Should detect arb with neg_risk=true market");
        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo);
    }

    /// Test: detects clear cross-platform arb (Kalshi YES + Poly NO)
    #[test]
    fn test_detects_kalshi_yes_poly_no_arb() {
        // Kalshi YES 40¢ + Poly NO 50¢ = 90¢ raw
        // Kalshi fee on 40¢ = 2¢
        // Effective = 92¢ → ARB!
        let (state, market_id) = setup_market(40, 65, 55, 50);

        let market = state.get_by_id(market_id).unwrap();
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        let arb = arb.expect("Should detect arb");
        assert_eq!(arb.arb_type, ArbType::KalshiYesPolyNo, "Should detect Kalshi YES + Poly NO arb");
    }

    /// Test: detects Polymarket-only arb (no fees)
    #[test]
    fn test_detects_poly_only_arb() {
        // Poly YES 48¢ + Poly NO 50¢ = 98¢ → 2% profit with ZERO fees!
        let (state, market_id) = setup_market(60, 60, 48, 50);

        let market = state.get_by_id(market_id).unwrap();
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        let arb = arb.expect("Should detect arb");
        assert_eq!(arb.arb_type, ArbType::PolyOnly, "Should detect Poly-only arb");
    }

    /// Test: detects Kalshi-only arb (double fees)
    #[test]
    fn test_detects_kalshi_only_arb() {
        // Kalshi YES 44¢ + Kalshi NO 44¢ = 88¢ raw
        // Double fee: ~4¢
        // Effective = 92¢ → ARB!
        let (state, market_id) = setup_market(44, 44, 60, 60);

        let market = state.get_by_id(market_id).unwrap();
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        let arb = arb.expect("Should detect arb");
        assert_eq!(arb.arb_type, ArbType::KalshiOnly, "Should detect Kalshi-only arb");
    }

    /// Test: correctly rejects marginal arb when fees eliminate profit
    #[test]
    fn test_fees_eliminate_marginal_arb() {
        // Poly YES 49¢ + Kalshi NO 50¢ = 99¢ raw
        // Kalshi fee on 50¢ = 2¢
        // Effective = 101¢ → NOT AN ARB (costs more than $1 payout!)
        let (state, market_id) = setup_market(55, 50, 49, 55);

        let market = state.get_by_id(market_id).unwrap();
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        // The arb might still be valid (PolyOnly might work), but PolyYesKalshiNo should not
        // If valid, it should NOT be PolyYesKalshiNo since fees eliminate it
        if let Some(arb) = arb {
            assert_ne!(arb.arb_type, ArbType::PolyYesKalshiNo,
                "Fees should eliminate marginal Poly YES + Kalshi NO arb");
        }
    }

    /// Test: returns no arbs for efficient market
    #[test]
    fn test_no_arbs_in_efficient_market() {
        // All prices sum to > $1
        let (state, market_id) = setup_market(55, 55, 52, 52);

        let market = state.get_by_id(market_id).unwrap();
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        assert!(arb.is_none(), "Should detect no arbs in efficient market");
    }

    /// Test: handles missing prices correctly
    #[test]
    fn test_handles_missing_prices() {
        let (state, market_id) = setup_market(50, 0, 50, 50);

        let market = state.get_by_id(market_id).unwrap();
        let arb = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        assert!(arb.is_none(), "Should return None when any price is missing");
    }

    // =========================================================================
    // Fee Calculation Tests (integer arithmetic)
    // =========================================================================

    /// Test: Integer fee calculation matches expected values
    #[test]
    fn test_kalshi_fee_accuracy() {
        // At 50¢: ceil(7 * 50 * 50 / 10000) = ceil(1.75) = 2
        assert_eq!(kalshi_fee(50), 2, "Fee at 50¢ should be 2¢");

        // At 10¢: ceil(7 * 10 * 90 / 10000) = ceil(0.63) = 1
        assert_eq!(kalshi_fee(10), 1, "Fee at 10¢ should be 1¢");

        // At 90¢: symmetric with 10¢
        assert_eq!(kalshi_fee(90), 1, "Fee at 90¢ should be 1¢");
    }

    // Note: test_fee_matches_legacy_for_common_prices was removed because
    // it tested against the deleted MarketArbState::kalshi_fee method.
    // The fee calculation (kalshi_fee_cents) is tested independently above.

    // =========================================================================
    // ArbOpportunity Tests
    // =========================================================================

    /// Test: ArbOpportunity calculates profit correctly
    #[test]
    fn test_execution_request_profit_calculation() {
        // Poly YES 40¢ + Kalshi NO 50¢ = 90¢
        // Kalshi fee on 50¢ = 2¢
        // Profit = 100 - 90 - 2 = 8¢
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.profit_cents(), 8, "Profit should be 8¢");
    }

    /// Test: ArbOpportunity handles negative profit
    #[test]
    fn test_execution_request_negative_profit() {
        // Prices too high - no profit
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 52,
            no_price: 52,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert!(req.profit_cents() < 0, "Should calculate negative profit");
    }

    // =========================================================================
    // GlobalStateLookup Tests
    // =========================================================================

    /// Test: GlobalStatelookup by Kalshi ticker hash
    #[test]
    fn test_lookup_by_kalshi_hash() {
        let (state, market_id) = setup_market(50, 50, 50, 50);

        let kalshi_hash = fxhash_str("KXEPLGAME-25DEC27CFCARS-CFC");
        let found_id = state.id_by_kalshi_hash(kalshi_hash);

        assert_eq!(found_id, Some(market_id), "Should find market by Kalshi hash");
    }

    /// Test: GlobalStatelookup by Poly token hashes
    #[test]
    fn test_lookup_by_poly_hashes() {
        let (state, market_id) = setup_market(50, 50, 50, 50);

        let poly_yes_hash = fxhash_str("arb_yes_token");
        let poly_no_hash = fxhash_str("arb_no_token");

        assert_eq!(state.id_by_poly_yes_hash(poly_yes_hash), Some(market_id));
        assert_eq!(state.id_by_poly_no_hash(poly_no_hash), Some(market_id));
    }

    /// Test: GlobalState handles multiple markets
    #[test]
    fn test_multiple_markets() {
        let state = GlobalState::default();

        // Add 5 markets
        for i in 0..5 {
            let pair = MarketPair {
                pair_id: format!("market-{}", i).into(),
                league: "epl".into(),
                market_type: MarketType::Moneyline,
                description: format!("Market {}", i).into(),
                kalshi_event_ticker: format!("KXTEST-{}", i).into(),
                kalshi_market_ticker: format!("KXTEST-{}-YES", i).into(),
                kalshi_event_slug: "premier-league-game".into(),
                poly_slug: format!("test-{}", i).into(),
                poly_yes_token: format!("yes_{}", i).into(),
                poly_no_token: format!("no_{}", i).into(),
                line_value: None,
                team_suffix: None,
                neg_risk: false,
            };

            let id = state.add_pair(pair).unwrap();
            assert_eq!(id, i as u16);
        }

        assert_eq!(state.market_count(), 5);

        // All should be findable
        for i in 0..5 {
            assert!(state.get_by_id(i as u16).is_some());
            assert_eq!(state.id_by_kalshi_hash(fxhash_str(&format!("KXTEST-{}-YES", i))), Some(i as u16));
        }
    }

    // =========================================================================
    // Price Conversion Tests
    // =========================================================================

    /// Test: Price conversion roundtrip
    #[test]
    fn test_price_conversion_roundtrip() {
        for cents in [1u16, 10, 25, 50, 75, 90, 99] {
            let price = cents_to_price(cents);
            let back = price_to_cents(price);
            assert_eq!(back, cents, "Roundtrip failed for {}¢", cents);
        }
    }

    /// Test: Fast price parsing
    #[test]
    fn test_parse_price_accuracy() {
        assert_eq!(parse_price("0.50"), 50);
        assert_eq!(parse_price("0.01"), 1);
        assert_eq!(parse_price("0.99"), 99);
        assert_eq!(parse_price("0.5"), 50);  // Short format
        assert_eq!(parse_price("invalid"), 0);  // Invalid
    }

    // =========================================================================
    // Full Flow Integration Test
    // =========================================================================

    /// Test: Complete arb detection → execution request flow
    #[test]
    fn test_complete_arb_flow() {
        // 1. Setup market with arb opportunity
        let (state, market_id) = setup_market(55, 50, 40, 65);

        // 2. Detect arb using ArbOpportunity::detect()
        let market = state.get_by_id(market_id).unwrap();
        let req = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        let req = req.expect("Step 2: Should detect arb");
        assert_eq!(req.arb_type, ArbType::PolyYesKalshiNo, "Should be PolyYesKalshiNo arb");

        // 3. Verify request is valid
        assert_eq!(req.yes_price, 40, "YES price should be 40¢");
        assert_eq!(req.no_price, 50, "NO price should be 50¢");
        assert!(req.profit_cents() > 0, "Should have positive profit");

        // 4. Verify we can access market pair for execution
        let pair_opt = market.pair();
        let pair = pair_opt.as_ref().expect("Should have pair");
        assert!(!pair.kalshi_market_ticker.is_empty());
        assert!(!pair.poly_yes_token.is_empty());
        assert!(!pair.poly_no_token.is_empty());
    }

    // Note: test_vs_legacy_arb_detection_consistency was removed because
    // it tested against the deleted MarketArbState, PlatformState, and MarketSide types.
    // The arb detection (check_arbs) is tested comprehensively above.
}

// ============================================================================
// EXECUTION ENGINE TESTS - Test execution without real API calls
// ============================================================================

mod execution_tests {
    use arb_bot::types::*;
    use arb_bot::circuit_breaker::*;
    use arb_bot::position_tracker::*;

    /// Test: ExecutionEngine correctly filters low-profit opportunities
    #[tokio::test]
    async fn test_execution_profit_threshold() {
        // This tests the logic flow - actual execution would need mocked clients
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 50,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // 50 + 50 + 2 (fee) = 102 > 100 → negative profit
        assert!(req.profit_cents() < 0, "Should have negative profit");
    }

    /// Test: ExecutionEngine respects circuit breaker
    #[tokio::test]
    async fn test_circuit_breaker_integration() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 50.0,
            max_total_position: 200.0,
            max_daily_loss: 25.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // Fill up market position (45 contracts at 50c each leg = $45 cost basis)
        cb.record_success("market1", 45, 50, 45, 50, 1.0).await;

        // Should block when adding $10 more (would exceed $50 limit)
        let result = cb.can_execute("market1", 10.0).await;
        assert!(matches!(result, Err(TripReason::MaxPositionPerMarket { .. })));
    }

    /// Test: Position tracker records fills correctly
    #[tokio::test]
    async fn test_position_tracker_integration() {
        let mut tracker = PositionTracker::new();

        // Simulate fill recording (what ExecutionEngine does)
        tracker.record_fill(&FillRecord::new(
            "test-market-1",
            "Test Market",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,  // fees
            "test_order_123",
        ));

        tracker.record_fill(&FillRecord::new(
            "test-market-1",
            "Test Market",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "test_order_456",
        ));

        let summary = tracker.summary();
        assert_eq!(summary.open_positions, 1);
        assert!(summary.total_guaranteed_profit > 0.0);
    }

    /// Test: NanoClock provides monotonic timing
    #[test]
    fn test_nano_clock_monotonic() {
        use arb_bot::execution::NanoClock;

        let clock = NanoClock::new();

        let t1 = clock.now_ns();
        std::thread::sleep(std::time::Duration::from_micros(100));
        let t2 = clock.now_ns();

        assert!(t2 > t1, "Clock should be monotonic");
        assert!(t2 - t1 >= 100_000, "Should measure at least 100µs");
    }
}

// ============================================================================
// MISMATCHED FILL / AUTO-CLOSE EXPOSURE TESTS
// ============================================================================
// These tests verify that when Kalshi and Polymarket fill different quantities,
// the system correctly handles the unmatched exposure.

mod mismatched_fill_tests {
    use arb_bot::position_tracker::*;

    /// Test: When Poly fills more than Kalshi, we have excess Poly exposure
    /// that needs to be sold to close the position.
    #[test]
    fn test_poly_fills_more_than_kalshi_creates_exposure() {
        let mut tracker = PositionTracker::new();

        // Scenario: Requested 10 contracts
        // Kalshi filled: 7 contracts at 50¢ (NO side)
        // Poly filled: 10 contracts at 40¢ (YES side)
        // Excess: 3 Poly YES contracts that aren't hedged

        // Record Kalshi fill (only 7)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            7.0,      // Only 7 filled
            0.50,
            0.12,     // fees
            "kalshi_order_1",
        ));

        // Record Poly fill (full 10)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            10.0,     // Full 10 filled
            0.40,
            0.0,
            "poly_order_1",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Matched position: 7 contracts
        // Unmatched Poly YES: 3 contracts
        assert!((pos.poly_yes.contracts - 10.0).abs() < 0.01, "Poly YES should have 10 contracts");
        assert!((pos.kalshi_no.contracts - 7.0).abs() < 0.01, "Kalshi NO should have 7 contracts");

        // This position has EXPOSURE because poly_yes (10) != kalshi_no (7)
        // The 7 matched contracts are hedged (guaranteed profit)
        // The 3 excess poly_yes contracts are unhedged exposure
    }

    /// Test: When Kalshi fills more than Poly, we have excess Kalshi exposure
    /// that needs to be sold to close the position.
    #[test]
    fn test_kalshi_fills_more_than_poly_creates_exposure() {
        let mut tracker = PositionTracker::new();

        // Scenario: Requested 10 contracts
        // Kalshi filled: 10 contracts at 50¢ (NO side)
        // Poly filled: 6 contracts at 40¢ (YES side)
        // Excess: 4 Kalshi NO contracts that aren't hedged

        // Record Kalshi fill (full 10)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,     // Full 10 filled
            0.50,
            0.18,     // fees
            "kalshi_order_1",
        ));

        // Record Poly fill (only 6)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            6.0,      // Only 6 filled
            0.40,
            0.0,
            "poly_order_1",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        assert!((pos.kalshi_no.contracts - 10.0).abs() < 0.01, "Kalshi NO should have 10 contracts");
        assert!((pos.poly_yes.contracts - 6.0).abs() < 0.01, "Poly YES should have 6 contracts");

        // The 6 matched contracts are hedged
        // The 4 excess kalshi_no contracts are unhedged exposure
    }

    /// Test: After auto-closing excess Poly, position should be balanced
    #[test]
    fn test_auto_close_poly_excess_balances_position() {
        let mut tracker = PositionTracker::new();

        // Initial mismatched fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            7.0,
            0.50,
            0.12,
            "kalshi_order_1",
        ));

        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "poly_order_1",
        ));

        // Simulate auto-close: SELL 3 Poly YES to close exposure
        // (In real execution, this would be a market order to dump the excess)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            -3.0,     // Negative = selling/reducing position
            0.38,     // Might get worse price on the close
            0.0,
            "poly_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // After auto-close, both sides should have 7 contracts
        assert!(
            (pos.poly_yes.contracts - 7.0).abs() < 0.01,
            "Poly YES should be reduced to 7 contracts, got {}",
            pos.poly_yes.contracts
        );
        assert!(
            (pos.kalshi_no.contracts - 7.0).abs() < 0.01,
            "Kalshi NO should still have 7 contracts, got {}",
            pos.kalshi_no.contracts
        );
    }

    /// Test: After auto-closing excess Kalshi, position should be balanced
    #[test]
    fn test_auto_close_kalshi_excess_balances_position() {
        let mut tracker = PositionTracker::new();

        // Initial mismatched fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,
            "kalshi_order_1",
        ));

        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            6.0,
            0.40,
            0.0,
            "poly_order_1",
        ));

        // Simulate auto-close: SELL 4 Kalshi NO to close exposure
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            -4.0,     // Negative = selling/reducing position
            0.48,     // Might get worse price on the close
            0.07,     // Still pay fees
            "kalshi_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // After auto-close, both sides should have 6 contracts
        assert!(
            (pos.kalshi_no.contracts - 6.0).abs() < 0.01,
            "Kalshi NO should be reduced to 6 contracts, got {}",
            pos.kalshi_no.contracts
        );
        assert!(
            (pos.poly_yes.contracts - 6.0).abs() < 0.01,
            "Poly YES should still have 6 contracts, got {}",
            pos.poly_yes.contracts
        );
    }

    /// Test: Complete failure on one side creates full exposure
    /// (e.g., Kalshi fills 10, Poly fills 0)
    #[test]
    fn test_complete_one_side_failure_full_exposure() {
        let mut tracker = PositionTracker::new();

        // Kalshi succeeds
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,
            "kalshi_order_1",
        ));

        // Poly completely fails - no fill recorded

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Full Kalshi exposure - must be closed immediately
        assert!((pos.kalshi_no.contracts - 10.0).abs() < 0.01);
        assert!((pos.poly_yes.contracts - 0.0).abs() < 0.01);

        // This is a dangerous situation - 10 unhedged Kalshi NO contracts
        // Auto-close should sell all 10 Kalshi NO to eliminate exposure
    }

    /// Test: Auto-close after complete one-side failure
    #[test]
    fn test_auto_close_after_complete_failure() {
        let mut tracker = PositionTracker::new();

        // Kalshi fills, Poly fails completely
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,
            "kalshi_order_1",
        ));

        // Auto-close: Sell ALL 10 Kalshi NO contracts
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            -10.0,    // Sell everything
            0.45,     // Might get a bad price in emergency close
            0.18,     // More fees
            "kalshi_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Position should be flat (0 contracts on both sides)
        assert!(
            (pos.kalshi_no.contracts - 0.0).abs() < 0.01,
            "Kalshi NO should be 0 after emergency close, got {}",
            pos.kalshi_no.contracts
        );
    }

    /// Test: Profit calculation with partial fill and auto-close
    #[test]
    fn test_profit_with_partial_fill_and_auto_close() {
        let mut tracker = PositionTracker::new();

        // Requested 10 contracts
        // Kalshi fills 8 @ 50¢ (cost: $4.00 + 0.14 fees)
        // Poly fills 10 @ 40¢ (cost: $4.00)
        // Need to close 2 excess Poly @ 38¢ (receive: $0.76)

        // Initial fills
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            8.0,
            0.50,
            0.14,
            "kalshi_order_1",
        ));

        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "poly_order_1",
        ));

        // Auto-close 2 excess Poly
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            -2.0,
            0.38,     // Sold at 38¢ (worse than 40¢ buy price)
            0.0,
            "poly_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Net position: 8 matched contracts
        // Kalshi NO: 8 @ 50¢ = $4.00 cost + $0.14 fees
        // Poly YES: 8 @ ~40¢ = ~$3.20 cost (10*0.40 - 2*0.38 = 4.00 - 0.76 = 3.24 effective for 8)

        assert!(
            (pos.kalshi_no.contracts - 8.0).abs() < 0.01,
            "Should have 8 matched Kalshi NO"
        );
        assert!(
            (pos.poly_yes.contracts - 8.0).abs() < 0.01,
            "Should have 8 matched Poly YES"
        );

        // The matched 8 contracts have guaranteed profit:
        // $1.00 payout - $0.50 kalshi - ~$0.405 poly = ~$0.095 per contract
        // But we also lost $0.02 per contract on the 2 we had to close (40¢ - 38¢)
    }
}

// ============================================================================
// PROCESS MOCK TESTS - Simulate execution flow without real APIs
// ============================================================================
// These tests verify that process correctly:
// 1. Records fills to the position tracker
// 2. Handles mismatched fills
// 3. Updates circuit breaker state
// 4. Captures order IDs from both platforms

mod process_mock_tests {
    use arb_bot::types::*;
    use arb_bot::circuit_breaker::*;
    use arb_bot::position_tracker::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Simulates what process does after execute_both_legs_async returns
    /// This allows testing the position tracking logic without real API clients
    struct MockExecutionResult {
        kalshi_filled: i64,
        poly_filled: i64,
        kalshi_cost: i64,  // cents
        poly_cost: i64,    // cents
        kalshi_order_id: String,
        poly_order_id: String,
    }

    /// Simulates the position tracking logic from process
    async fn simulate_process_position_tracking(
        tracker: &Arc<RwLock<PositionTracker>>,
        circuit_breaker: &CircuitBreaker,
        pair: &MarketPair,
        req: &ArbOpportunity,
        result: MockExecutionResult,
    ) -> (i64, i16) {  // Returns (matched, profit_cents)
        let matched = result.kalshi_filled.min(result.poly_filled);
        let actual_profit = matched as i16 * 100 - (result.kalshi_cost + result.poly_cost) as i16;

        // Determine sides for position tracking
        let (kalshi_side, poly_side) = match req.arb_type {
            ArbType::PolyYesKalshiNo => ("no", "yes"),
            ArbType::KalshiYesPolyNo => ("yes", "no"),
            ArbType::PolyOnly => ("", "both"),
            ArbType::KalshiOnly => ("both", ""),
        };

        // Record success to circuit breaker
        if matched > 0 {
            let kalshi_price_cents = if result.kalshi_filled > 0 { result.kalshi_cost / result.kalshi_filled } else { 0 };
            let poly_price_cents = if result.poly_filled > 0 { result.poly_cost / result.poly_filled } else { 0 };
            circuit_breaker.record_success(&pair.pair_id, matched, kalshi_price_cents, matched, poly_price_cents, actual_profit as f64 / 100.0).await;
        }

        // === UPDATE POSITION TRACKER (mirrors process logic) ===
        if matched > 0 || result.kalshi_filled > 0 || result.poly_filled > 0 {
            let mut tracker_guard = tracker.write().await;

            // Record Kalshi fill
            if result.kalshi_filled > 0 {
                tracker_guard.record_fill(&FillRecord::new(
                    &pair.pair_id,
                    &pair.description,
                    "kalshi",
                    kalshi_side,
                    matched as f64,
                    result.kalshi_cost as f64 / 100.0 / result.kalshi_filled.max(1) as f64,
                    0.0,
                    &result.kalshi_order_id,
                ));
            }

            // Record Poly fill
            if result.poly_filled > 0 {
                tracker_guard.record_fill(&FillRecord::new(
                    &pair.pair_id,
                    &pair.description,
                    "polymarket",
                    poly_side,
                    matched as f64,
                    result.poly_cost as f64 / 100.0 / result.poly_filled.max(1) as f64,
                    0.0,
                    &result.poly_order_id,
                ));
            }

            tracker_guard.save_async();
        }

        (matched, actual_profit)
    }

    fn test_market_pair() -> MarketPair {
        MarketPair {
            pair_id: "process-fast-test".into(),
            league: "epl".into(),
            market_type: MarketType::Moneyline,
            description: "Process Fast Test Market".into(),
            kalshi_event_ticker: "KXTEST-PROCESS".into(),
            kalshi_market_ticker: "KXTEST-PROCESS-YES".into(),
            kalshi_event_slug: "premier-league-game".into(),
            poly_slug: "process-fast-test".into(),
            poly_yes_token: "pf_yes_token".into(),
            poly_no_token: "pf_no_token".into(),
            line_value: None,
            team_suffix: None,
            neg_risk: false,
        }
    }

    fn test_circuit_breaker_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 50.0,
            max_consecutive_errors: 5,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        }
    }

    /// Test: process records both fills to position tracker with correct order IDs
    #[tokio::test]
    async fn test_process_records_fills_with_order_ids() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,  // 10 contracts × 50¢
            poly_cost: 400,    // 10 contracts × 40¢
            kalshi_order_id: "kalshi_order_abc123".to_string(),
            poly_order_id: "poly_order_xyz789".to_string(),
        };

        let (matched, profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Verify matched contracts
        assert_eq!(matched, 10, "Should have 10 matched contracts");

        // Verify profit: 10 contracts × $1 payout - $5 Kalshi - $4 Poly = $1 = 100¢
        assert_eq!(profit, 100, "Should have 100¢ profit");

        // Verify position tracker was updated
        let tracker_guard = tracker.read().await;
        let summary = tracker_guard.summary();

        assert_eq!(summary.open_positions, 1, "Should have 1 open position");
        assert!(summary.total_contracts > 0.0, "Should have contracts recorded");

        // Verify the position has both legs recorded
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");
        assert!(pos.kalshi_no.contracts > 0.0, "Should have Kalshi NO contracts");
        assert!(pos.poly_yes.contracts > 0.0, "Should have Poly YES contracts");
    }

    /// Test: process handles Poly YES + Kalshi NO correctly (sides)
    #[tokio::test]
    async fn test_process_poly_yes_kalshi_no_sides() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        // Poly YES + Kalshi NO configuration
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,
            poly_cost: 400,
            kalshi_order_id: "k_order_1".to_string(),
            poly_order_id: "p_order_1".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;

        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // With arb_type = PolyYesKalshiNo:
        // - Kalshi side = "no"
        // - Poly side = "yes"
        assert!((pos.kalshi_no.contracts - 10.0).abs() < 0.01, "Kalshi NO should have 10 contracts");
        assert!((pos.poly_yes.contracts - 10.0).abs() < 0.01, "Poly YES should have 10 contracts");
        assert!((pos.kalshi_yes.contracts - 0.0).abs() < 0.01, "Kalshi YES should be empty");
        assert!((pos.poly_no.contracts - 0.0).abs() < 0.01, "Poly NO should be empty");
    }

    /// Test: process handles Kalshi YES + Poly NO correctly (reversed sides)
    #[tokio::test]
    async fn test_process_kalshi_yes_poly_no_sides() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        // Kalshi YES + Poly NO configuration
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
            is_test: false,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 400,  // Kalshi YES at 40¢
            poly_cost: 500,    // Poly NO at 50¢
            kalshi_order_id: "k_order_2".to_string(),
            poly_order_id: "p_order_2".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;

        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // With arb_type = KalshiYesPolyNo:
        // - Kalshi side = "yes"
        // - Poly side = "no"
        assert!((pos.kalshi_yes.contracts - 10.0).abs() < 0.01, "Kalshi YES should have 10 contracts");
        assert!((pos.poly_no.contracts - 10.0).abs() < 0.01, "Poly NO should have 10 contracts");
        assert!((pos.kalshi_no.contracts - 0.0).abs() < 0.01, "Kalshi NO should be empty");
        assert!((pos.poly_yes.contracts - 0.0).abs() < 0.01, "Poly YES should be empty");
    }

    /// Test: process updates circuit breaker on success
    #[tokio::test]
    async fn test_process_updates_circuit_breaker() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,
            poly_cost: 400,
            kalshi_order_id: "k_order_3".to_string(),
            poly_order_id: "p_order_3".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;

        // Verify circuit breaker was updated
        let status = cb.status().await;
        assert_eq!(status.consecutive_errors, 0, "Errors should be reset after success");
        assert!(status.total_position > 0.0, "Total position should be tracked");
    }

    /// Test: process handles partial Kalshi fill correctly
    #[tokio::test]
    async fn test_process_partial_kalshi_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // Kalshi only fills 7 out of 10
        let result = MockExecutionResult {
            kalshi_filled: 7,
            poly_filled: 10,
            kalshi_cost: 350,  // 7 × 50¢
            poly_cost: 400,    // 10 × 40¢
            kalshi_order_id: "k_partial".to_string(),
            poly_order_id: "p_full".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Matched should be min(7, 10) = 7
        assert_eq!(matched, 7, "Matched should be min of both fills");

        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // Position tracker records matched amounts (7), not total fills
        assert!((pos.kalshi_no.contracts - 7.0).abs() < 0.01, "Should record matched Kalshi contracts");
        assert!((pos.poly_yes.contracts - 7.0).abs() < 0.01, "Should record matched Poly contracts");
    }

    /// Test: process handles partial Poly fill correctly
    #[tokio::test]
    async fn test_process_partial_poly_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // Poly only fills 6 out of 10
        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 6,
            kalshi_cost: 500,  // 10 × 50¢
            poly_cost: 240,    // 6 × 40¢
            kalshi_order_id: "k_full".to_string(),
            poly_order_id: "p_partial".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Matched should be min(10, 6) = 6
        assert_eq!(matched, 6, "Matched should be min of both fills");
    }

    /// Test: process handles zero Kalshi fill
    #[tokio::test]
    async fn test_process_zero_kalshi_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // Kalshi fills 0, Poly fills 10 (complete failure on one side)
        let result = MockExecutionResult {
            kalshi_filled: 0,
            poly_filled: 10,
            kalshi_cost: 0,
            poly_cost: 400,
            kalshi_order_id: "".to_string(),
            poly_order_id: "p_only".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // No matched contracts since one side is 0
        assert_eq!(matched, 0, "No matched contracts when one side is 0");

        // But position tracker still records the Poly fill (for exposure tracking)
        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position even with partial fills");

        // Poly fill should still be recorded (matched=0 means 0 recorded as matched)
        // The position exists but has 0 matched contracts
    }

    /// Test: process handles zero Poly fill
    #[tokio::test]
    async fn test_process_zero_poly_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // Kalshi fills 10, Poly fills 0
        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 0,
            kalshi_cost: 500,
            poly_cost: 0,
            kalshi_order_id: "k_only".to_string(),
            poly_order_id: "".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        assert_eq!(matched, 0, "No matched contracts when Poly is 0");
    }

    /// Test: process correctly calculates profit with full fills
    #[tokio::test]
    async fn test_process_profit_calculation_full_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,  // Poly YES at 40¢
            no_price: 50,   // Kalshi NO at 50¢
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,  // 10 × 50¢ = $5.00 = 500¢
            poly_cost: 400,    // 10 × 40¢ = $4.00 = 400¢
            kalshi_order_id: "k_profit".to_string(),
            poly_order_id: "p_profit".to_string(),
        };

        let (matched, profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Profit = matched × $1 payout - costs
        // = 10 × 100¢ - 500¢ - 400¢
        // = 1000¢ - 900¢
        // = 100¢
        assert_eq!(matched, 10);
        assert_eq!(profit, 100, "Profit should be 100¢ ($1.00)");
    }

    /// Test: process correctly calculates profit with partial fill
    #[tokio::test]
    async fn test_process_profit_calculation_partial_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // Partial fill: Kalshi 7, Poly 10
        let result = MockExecutionResult {
            kalshi_filled: 7,
            poly_filled: 10,
            kalshi_cost: 350,  // 7 × 50¢
            poly_cost: 400,    // 10 × 40¢ (but only 7 are matched)
            kalshi_order_id: "k_partial_profit".to_string(),
            poly_order_id: "p_partial_profit".to_string(),
        };

        let (matched, profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Profit = matched × $1 payout - ALL costs (including unmatched)
        // = 7 × 100¢ - 350¢ - 400¢
        // = 700¢ - 750¢
        // = -50¢ (LOSS because we paid for 10 Poly but only matched 7!)
        assert_eq!(matched, 7);
        assert_eq!(profit, -50, "Should have -50¢ loss due to unmatched Poly contracts");
    }

    /// Test: Multiple executions accumulate in position tracker
    #[tokio::test]
    async fn test_process_multiple_executions_accumulate() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // First execution: 10 contracts
        let result1 = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,
            poly_cost: 400,
            kalshi_order_id: "k_exec_1".to_string(),
            poly_order_id: "p_exec_1".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result1).await;

        // Second execution: 5 more contracts
        let result2 = MockExecutionResult {
            kalshi_filled: 5,
            poly_filled: 5,
            kalshi_cost: 250,
            poly_cost: 200,
            kalshi_order_id: "k_exec_2".to_string(),
            poly_order_id: "p_exec_2".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result2).await;

        // Verify accumulated position
        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // Should have 15 total contracts (10 + 5)
        assert!(
            (pos.kalshi_no.contracts - 15.0).abs() < 0.01,
            "Should have accumulated 15 Kalshi NO contracts, got {}",
            pos.kalshi_no.contracts
        );
        assert!(
            (pos.poly_yes.contracts - 15.0).abs() < 0.01,
            "Should have accumulated 15 Poly YES contracts, got {}",
            pos.poly_yes.contracts
        );
    }

    /// Test: Circuit breaker tracks accumulated position per market
    #[tokio::test]
    async fn test_circuit_breaker_accumulates_position() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        // Execute multiple times
        for i in 0..5 {
            let result = MockExecutionResult {
                kalshi_filled: 10,
                poly_filled: 10,
                kalshi_cost: 500,
                poly_cost: 400,
                kalshi_order_id: format!("k_cb_{}", i),
                poly_order_id: format!("p_cb_{}", i),
            };

            simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;
        }

        // Circuit breaker tracks dollar cost basis on BOTH sides (kalshi + poly)
        // 5 executions × (10 contracts × 50c/100 + 10 contracts × 40c/100) = 5 × $9.0 = $45.0
        let status = cb.status().await;
        assert!((status.total_position - 45.0).abs() < 0.01, "Circuit breaker should track $45.0 total cost basis, got {}", status.total_position);
    }

    // =========================================================================
    // SAME-PLATFORM ARB TESTS (PolyOnly and KalshiOnly)
    // =========================================================================

    /// Test: PolyOnly arb (Poly YES + Poly NO on same platform - zero Kalshi fees)
    #[tokio::test]
    async fn test_process_poly_only_arb() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        // PolyOnly: Buy YES and NO both on Polymarket
        // This is unusual but profitable when Poly YES + Poly NO < $1
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 48,  // Poly YES at 48¢
            no_price: 50,   // Poly NO at 50¢ (total = 98¢, 2¢ profit with NO fees!)
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyOnly,
            detected_ns: 0,
            is_test: false,
        };

        // For PolyOnly, both fills are from Polymarket
        // In real execution: leg1 = Poly YES, leg2 = Poly NO
        let result = MockExecutionResult {
            kalshi_filled: 0,   // No Kalshi in PolyOnly
            poly_filled: 10,    // Both YES and NO filled on Poly (combined)
            kalshi_cost: 0,
            poly_cost: 980,     // 10 × (48 + 50) = 980¢
            kalshi_order_id: "".to_string(),
            poly_order_id: "poly_both_order".to_string(),
        };

        // Note: PolyOnly doesn't fit our mock perfectly since it has 2 Poly fills
        // But we can verify the ArbType is handled correctly
        assert_eq!(req.estimated_fee_cents(), 0, "PolyOnly should have ZERO fees");
        assert_eq!(req.profit_cents(), 2, "PolyOnly profit = 100 - 48 - 50 - 0 = 2¢");
    }

    /// Test: KalshiOnly arb (Kalshi YES + Kalshi NO on same platform - double fees)
    #[tokio::test]
    async fn test_process_kalshi_only_arb() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        // KalshiOnly: Buy YES and NO both on Kalshi
        // Must overcome DOUBLE fees (fee on YES side + fee on NO side)
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 44,  // Kalshi YES at 44¢
            no_price: 44,   // Kalshi NO at 44¢ (raw = 88¢)
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiOnly,
            detected_ns: 0,
            is_test: false,
        };

        // Double fee: kalshi_fee(44) + kalshi_fee(44)
        // kalshi_fee(44) = ceil(7 * 44 * 56 / 10000) = ceil(1.7248) = 2¢
        // Total fees = 2 + 2 = 4¢
        let expected_fees = arb_bot::arb::kalshi_fee(44) + arb_bot::arb::kalshi_fee(44);
        assert_eq!(req.estimated_fee_cents(), expected_fees, "KalshiOnly should have double fees");

        // Profit = 100 - 44 - 44 - 4 = 8¢
        assert_eq!(req.profit_cents(), 8, "KalshiOnly profit = 100 - 44 - 44 - 4 = 8¢");
    }

    /// Test: PolyOnly fee calculation is always zero
    #[test]
    fn test_poly_only_zero_fees() {
        for yes_price in [10u16, 25, 50, 75, 90] {
            for no_price in [10u16, 25, 50, 75, 90] {
                let req = ArbOpportunity {
                    market_id: 0,
                    yes_price,
                    no_price,
                    yes_size: 1000,
                    no_size: 1000,
                    arb_type: ArbType::PolyOnly,
                    detected_ns: 0,
                    is_test: false,
                };
                assert_eq!(req.estimated_fee_cents(), 0,
                    "PolyOnly should always have 0 fees, got {} for prices ({}, {})",
                    req.estimated_fee_cents(), yes_price, no_price);
            }
        }
    }

    /// Test: KalshiOnly fee calculation is double (fee on both YES and NO sides)
    #[test]
    fn test_kalshi_only_double_fees() {
        use arb_bot::arb::kalshi_fee;

        for yes_price in [10u16, 25, 50, 75, 90] {
            for no_price in [10u16, 25, 50, 75, 90] {
                let req = ArbOpportunity {
                    market_id: 0,
                    yes_price,
                    no_price,
                    yes_size: 1000,
                    no_size: 1000,
                    arb_type: ArbType::KalshiOnly,
                    detected_ns: 0,
                    is_test: false,
                };
                let expected = kalshi_fee(yes_price) + kalshi_fee(no_price);
                assert_eq!(req.estimated_fee_cents(), expected,
                    "KalshiOnly fees should be {} for prices ({}, {}), got {}",
                    expected, yes_price, no_price, req.estimated_fee_cents());
            }
        }
    }

    /// Test: Cross-platform fee calculation (fee only on Kalshi side)
    #[test]
    fn test_cross_platform_single_fee() {
        use arb_bot::arb::kalshi_fee;

        // PolyYesKalshiNo: fee on Kalshi NO side
        let req1 = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };
        assert_eq!(req1.estimated_fee_cents(), kalshi_fee(50),
            "PolyYesKalshiNo fee should be on NO side (50¢)");

        // KalshiYesPolyNo: fee on Kalshi YES side
        let req2 = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
            is_test: false,
        };
        assert_eq!(req2.estimated_fee_cents(), kalshi_fee(40),
            "KalshiYesPolyNo fee should be on YES side (40¢)");
    }

    /// Test: Profit comparison across all arb types with same prices
    #[test]
    fn test_profit_comparison_all_arb_types() {
        use arb_bot::arb::kalshi_fee;

        let yes_price = 45u16;
        let no_price = 45u16;
        // Raw cost = 90¢, payout = 100¢

        // PolyOnly: 0 fees → 10¢ profit
        let poly_only = ArbOpportunity {
            market_id: 0,
            yes_price,
            no_price,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyOnly,
            detected_ns: 0,
            is_test: false,
        };

        // KalshiOnly: double fees → less profit
        let kalshi_only = ArbOpportunity {
            market_id: 0,
            yes_price,
            no_price,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiOnly,
            detected_ns: 0,
            is_test: false,
        };

        // Cross-platform: single fee
        let cross1 = ArbOpportunity {
            market_id: 0,
            yes_price,
            no_price,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        let cross2 = ArbOpportunity {
            market_id: 0,
            yes_price,
            no_price,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
            is_test: false,
        };

        // PolyOnly should always be most profitable (no fees)
        assert!(poly_only.profit_cents() > kalshi_only.profit_cents(),
            "PolyOnly ({}¢) should be more profitable than KalshiOnly ({}¢)",
            poly_only.profit_cents(), kalshi_only.profit_cents());

        assert!(poly_only.profit_cents() >= cross1.profit_cents(),
            "PolyOnly ({}¢) should be >= cross-platform ({}¢)",
            poly_only.profit_cents(), cross1.profit_cents());

        // Cross-platform should be more profitable than KalshiOnly (single vs double fee)
        assert!(cross1.profit_cents() > kalshi_only.profit_cents(),
            "Cross-platform ({}¢) should be more profitable than KalshiOnly ({}¢)",
            cross1.profit_cents(), kalshi_only.profit_cents());

        // Both cross-platform types have same fees (one Kalshi side each)
        assert_eq!(cross1.profit_cents(), cross2.profit_cents(),
            "Both cross-platform types should have equal profit");
    }
}

// ============================================================================
// STARTUP SWEEP TESTS - Test the startup sweep logic for catching missed arbs
// ============================================================================

mod startup_sweep_tests {
    use arb_bot::types::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    // Mutex to serialize tests that modify env vars
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Helper: Create a test MarketPair
    fn make_pair(id: &str) -> MarketPair {
        MarketPair {
            pair_id: id.into(),
            league: "test".into(),
            market_type: MarketType::Moneyline,
            description: format!("Test Market {}", id).into(),
            kalshi_event_ticker: format!("KXTEST-{}", id).into(),
            kalshi_market_ticker: format!("KXTEST-{}-YES", id).into(),
            kalshi_event_slug: format!("test-{}", id).into(),
            poly_slug: format!("test-{}", id).into(),
            poly_yes_token: format!("yes-{}", id).into(),
            poly_no_token: format!("no-{}", id).into(),
            line_value: None,
            team_suffix: None,
            neg_risk: false,
        }
    }

    /// Helper: Create a GlobalState with multiple markets, some with arbs
    fn setup_markets_for_sweep() -> Arc<GlobalState> {
        let state = GlobalState::default();

        // Market 0: Has arb (Poly YES 40 + Kalshi NO 50 + fee 2 = 92)
        let id0 = state.add_pair(make_pair("0")).unwrap();
        let market0 = state.get_by_id(id0).unwrap();
        market0.kalshi.store(55, 50, 1000, 1000);  // Kalshi YES=55, NO=50
        market0.poly.store(40, 65, 1000, 1000);    // Poly YES=40, NO=65

        // Market 1: No arb (prices too high)
        let id1 = state.add_pair(make_pair("1")).unwrap();
        let market1 = state.get_by_id(id1).unwrap();
        market1.kalshi.store(55, 55, 1000, 1000);  // No arb
        market1.poly.store(55, 55, 1000, 1000);

        // Market 2: Incomplete prices (Poly not loaded)
        let id2 = state.add_pair(make_pair("2")).unwrap();
        let market2 = state.get_by_id(id2).unwrap();
        market2.kalshi.store(40, 50, 1000, 1000);  // Kalshi loaded
        market2.poly.store(0, 0, 0, 0);            // Poly NOT loaded

        // Market 3: Another arb (KalshiYesPolyNo)
        let id3 = state.add_pair(make_pair("3")).unwrap();
        let market3 = state.get_by_id(id3).unwrap();
        market3.kalshi.store(40, 65, 1000, 1000);  // Kalshi YES=40
        market3.poly.store(55, 50, 1000, 1000);    // Poly NO=50

        Arc::new(state)
    }

    /// Test: Startup sweep detects arbs in markets with complete prices
    #[tokio::test]
    async fn test_startup_sweep_detects_arbs() {
        let state = setup_markets_for_sweep();
        let (tx, mut rx) = mpsc::channel::<ArbOpportunity>(16);

        // Simulate startup sweep logic
        let market_count = state.market_count();
        let mut arbs_found = 0;
        let mut markets_scanned = 0;

        for market in state.markets.iter().take(market_count) {
            let kalshi_data = market.kalshi.load();
            let poly_data = market.poly.load();
            let (k_yes, k_no, _, _) = kalshi_data;
            let (p_yes, p_no, _, _) = poly_data;

            // Only check markets with both platforms populated
            if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
                continue;
            }
            markets_scanned += 1;

            if let Some(req) = ArbOpportunity::detect(
                market.market_id,
                kalshi_data,
                poly_data,
                state.arb_config(),
                0,
            ) {
                arbs_found += 1;
                tx.try_send(req).unwrap();
            }
        }

        // Should have scanned 3 markets (excluding the one with incomplete prices)
        assert_eq!(markets_scanned, 3, "Should scan 3 markets with complete prices");

        // Should have found 2 arbs (market 0 and market 3)
        assert_eq!(arbs_found, 2, "Should find 2 arb opportunities");

        // Verify the requests were sent
        let req1 = rx.try_recv().expect("Should receive first arb request");
        assert!(req1.profit_cents() > 0, "First arb should have positive profit");

        let req2 = rx.try_recv().expect("Should receive second arb request");
        assert!(req2.profit_cents() > 0, "Second arb should have positive profit");

        // No more requests
        assert!(rx.try_recv().is_err(), "Should only have 2 arb requests");
    }

    /// Test: Startup sweep skips markets with incomplete prices
    #[tokio::test]
    async fn test_startup_sweep_skips_incomplete_markets() {
        let state = GlobalState::default();

        // Market with only Kalshi prices
        let id = state.add_pair(make_pair("inc")).unwrap();
        let market = state.get_by_id(id).unwrap();

        // Only Kalshi prices set (simulates Kalshi loaded before Poly)
        market.kalshi.store(40, 50, 1000, 1000);
        market.poly.store(0, 0, 0, 0);  // Poly not loaded

        let (k_yes, k_no, _, _) = market.kalshi.load();
        let (p_yes, p_no, _, _) = market.poly.load();

        // Should be skipped due to incomplete prices
        let should_skip = k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0;
        assert!(should_skip, "Market with incomplete prices should be skipped");
    }

    /// Test: Startup sweep handles zero arb opportunities gracefully
    #[tokio::test]
    async fn test_startup_sweep_no_arbs() {
        let state = GlobalState::default();

        // Market with no arb (prices too high)
        let id = state.add_pair(make_pair("no-arb")).unwrap();
        let market = state.get_by_id(id).unwrap();
        market.kalshi.store(55, 55, 1000, 1000);
        market.poly.store(55, 55, 1000, 1000);

        let (_tx, mut rx) = mpsc::channel::<ArbOpportunity>(16);

        let market_count = state.market_count();
        let mut arbs_found = 0;

        for market in state.markets.iter().take(market_count) {
            let kalshi_data = market.kalshi.load();
            let poly_data = market.poly.load();
            let (k_yes, k_no, _, _) = kalshi_data;
            let (p_yes, p_no, _, _) = poly_data;

            if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
                continue;
            }

            if ArbOpportunity::detect(
                market.market_id,
                kalshi_data,
                poly_data,
                state.arb_config(),
                0,
            ).is_some() {
                arbs_found += 1;
            }
        }

        assert_eq!(arbs_found, 0, "Should find no arb opportunities");
        assert!(rx.try_recv().is_err(), "Channel should be empty");
    }

    /// Test: Startup sweep correctly prioritizes cross-platform arbs
    #[tokio::test]
    async fn test_startup_sweep_arb_type_priority() {
        let state = GlobalState::default();

        // Market with multiple arb types possible (cross-platform should win)
        let id = state.add_pair(make_pair("multi")).unwrap();
        let market = state.get_by_id(id).unwrap();

        // Set up prices where both PolyYesKalshiNo AND PolyOnly are arbs
        // Poly YES=40, Poly NO=50 = 90 (Poly only arb)
        // Poly YES=40, Kalshi NO=50 + fee 2 = 92 (Cross platform arb)
        market.kalshi.store(55, 50, 1000, 1000);
        market.poly.store(40, 50, 1000, 1000);

        let arb = ArbOpportunity::detect(
            id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            0,
        );

        // ArbOpportunity::detect() should pick the best arb type with priority
        let arb = arb.expect("Should detect arb");

        // PolyYesKalshiNo is checked first in priority order
        assert!(matches!(arb.arb_type, ArbType::PolyYesKalshiNo),
            "Cross-platform arb should take priority");
    }

    /// Test: Startup sweep routes arbs to confirm queue when league requires confirmation
    /// This test calls the actual `arb::sweep_markets` function used by main.rs
    #[tokio::test]
    async fn test_startup_sweep_routes_to_confirm_queue() {
        use arb_bot::arb::sweep_markets;

        let _lock = ENV_MUTEX.lock().unwrap();

        // Set up: "nba" skips confirmation, "epl" requires confirmation
        std::env::set_var("CONFIRM_MODE_SKIP", "nba");

        // Helper to create a pair with specific league
        fn make_league_pair(id: &str, league: &str) -> MarketPair {
            MarketPair {
                pair_id: id.into(),
                league: league.into(),
                market_type: MarketType::Moneyline,
                description: format!("Test Market {}", id).into(),
                kalshi_event_ticker: format!("KXTEST-{}", id).into(),
                kalshi_market_ticker: format!("KXTEST-{}-YES", id).into(),
                kalshi_event_slug: format!("test-{}", id).into(),
                poly_slug: format!("test-{}", id).into(),
                poly_yes_token: format!("yes-{}", id).into(),
                poly_no_token: format!("no-{}", id).into(),
                line_value: None,
                team_suffix: None,
                neg_risk: false,
            }
        }

        let state = GlobalState::default();

        // Market 0: NBA league (skips confirmation) - has arb
        let id0 = state.add_pair(make_league_pair("nba-game", "nba")).unwrap();
        let market0 = state.get_by_id(id0).unwrap();
        market0.kalshi.store(55, 50, 1000, 1000);
        market0.poly.store(40, 65, 1000, 1000);

        // Market 1: EPL league (requires confirmation) - has arb
        let id1 = state.add_pair(make_league_pair("epl-game", "epl")).unwrap();
        let market1 = state.get_by_id(id1).unwrap();
        market1.kalshi.store(55, 50, 1000, 1000);
        market1.poly.store(40, 65, 1000, 1000);

        // Two channels: exec for auto-execute, confirm for confirmation required
        let (exec_tx, mut exec_rx) = mpsc::channel::<ArbOpportunity>(16);
        let (confirm_tx, mut confirm_rx) = mpsc::channel::<(ArbOpportunity, Arc<MarketPair>)>(16);

        // Call the actual sweep_markets function used by main.rs
        let result = sweep_markets(&state, 0, &exec_tx, &confirm_tx);

        // Verify sweep result counts
        assert_eq!(result.markets_scanned, 2, "Should scan 2 markets");
        assert_eq!(result.arbs_found, 2, "Should find 2 arbs");
        assert_eq!(result.routed_to_exec, 1, "Should route 1 to exec (NBA)");
        assert_eq!(result.routed_to_confirm, 1, "Should route 1 to confirm (EPL)");

        // Verify NBA arb went to exec channel (no confirmation needed)
        let exec_arb = exec_rx.try_recv().expect("NBA arb should go to exec channel");
        assert_eq!(exec_arb.market_id, id0, "Exec channel should have NBA market");

        // Verify EPL arb went to confirm channel (confirmation required)
        let (confirm_arb, confirm_pair) = confirm_rx.try_recv()
            .expect("EPL arb should go to confirm channel");
        assert_eq!(confirm_arb.market_id, id1, "Confirm channel should have EPL market");
        assert_eq!(&*confirm_pair.league, "epl", "Confirm pair should be EPL");

        // No more items in either channel
        assert!(exec_rx.try_recv().is_err(), "Exec channel should be empty");
        assert!(confirm_rx.try_recv().is_err(), "Confirm channel should be empty");

        // Cleanup
        std::env::remove_var("CONFIRM_MODE_SKIP");
    }
}

// =============================================================================
// Kalshi Delta Correctness Tests (Issue #54 fix verification)
// =============================================================================

mod kalshi_delta_correctness {
    use arb_bot::types::{AtomicMarketState, ArbOpportunity};
    use arb_bot::arb::ArbConfig;

    /// Helper: simulate a snapshot by populating KalshiBook + AtomicOrderbook
    fn apply_snapshot(market: &AtomicMarketState, yes_bids: &[Vec<i64>], no_bids: &[Vec<i64>]) {
        let mut book = market.kalshi_book.lock();
        book.set_yes_bids(yes_bids);
        book.set_no_bids(no_bids);
        let (no_ask, no_size) = book.derive_no_side();
        let (yes_ask, yes_size) = book.derive_yes_side();
        drop(book);
        market.kalshi.store(yes_ask, no_ask, yes_size, no_size);
    }

    /// Helper: simulate a delta by applying to KalshiBook + updating AtomicOrderbook
    fn apply_delta(market: &AtomicMarketState, side: &str, price: i64, delta: i64) {
        let mut book = market.kalshi_book.lock();
        book.apply_delta(side, price, delta);
        match side {
            "yes" => {
                let (no_ask, no_size) = book.derive_no_side();
                drop(book);
                market.kalshi.update_no(no_ask, no_size);
            }
            "no" => {
                let (yes_ask, yes_size) = book.derive_yes_side();
                drop(book);
                market.kalshi.update_yes(yes_ask, yes_size);
            }
            _ => {}
        }
    }

    #[test]
    fn test_cancel_at_best_bid_clears_price() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23]], &[vec![60, 15]]);

        let (_, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 64);

        apply_delta(&market, "yes", 36, -23);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 0, "NO ask should be 0 when all YES bids are gone");
        assert_eq!(no_size, 0);
    }

    #[test]
    fn test_cancel_best_reveals_next_best() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23], vec![34, 10]], &[vec![60, 15]]);

        let (_, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 64);

        apply_delta(&market, "yes", 36, -23);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 66, "NO ask should fall back to 100 - 34 = 66");
        assert_eq!(no_size, 3); // 10 * 34 / 100 = 3
    }

    #[test]
    fn test_no_phantom_arb_after_cancel() {
        let market = AtomicMarketState::new(0);
        let config = ArbConfig::default();

        // YES bid at 36 with 2000 qty -> no_ask=64, no_size=720
        // NO bid at 60 with 500 qty -> yes_ask=40, yes_size=300
        apply_snapshot(&market, &[vec![36, 2000]], &[vec![60, 500]]);
        // Poly YES at 30c -> PolyYesKalshiNo cost = 30 + 64 + fee(64)=2 = 96 <= 99
        market.poly.store(30, 70, 500, 500);

        let arb = ArbOpportunity::detect(0, market.kalshi.load(), market.poly.load(), &config, 0);
        assert!(arb.is_some(), "Arb should exist before cancel");

        // Cancel all YES bids at 36 -> no_ask drops to 0
        apply_delta(&market, "yes", 36, -2000);

        let arb = ArbOpportunity::detect(0, market.kalshi.load(), market.poly.load(), &config, 0);
        assert!(arb.is_none(), "Phantom arb should NOT exist after bid cancellation");
    }

    #[test]
    fn test_delta_adds_new_level() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23]], &[vec![60, 15]]);

        apply_delta(&market, "yes", 40, 10);

        let (_, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 60, "NO ask should update to 100 - 40 = 60");
    }

    #[test]
    fn test_delta_partial_reduction() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23]], &[vec![60, 15]]);

        apply_delta(&market, "yes", 36, -10);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 64);
        assert_eq!(no_size, 4); // 13 * 36 / 100 = 4
    }

    #[test]
    fn test_non_best_level_delta_preserves_best() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23], vec![34, 10]], &[vec![60, 15]]);

        apply_delta(&market, "yes", 34, -5);

        let (_, no_ask, _, no_size) = market.kalshi.load();
        assert_eq!(no_ask, 64, "NO ask should still reflect best bid at 36");
        assert_eq!(no_size, 8); // 23 * 36 / 100 = 8
    }

    #[test]
    fn test_snapshot_resets_book() {
        let market = AtomicMarketState::new(0);
        apply_snapshot(&market, &[vec![36, 23], vec![34, 10]], &[vec![60, 15]]);

        apply_delta(&market, "yes", 36, -23);
        apply_delta(&market, "yes", 38, 50);

        apply_snapshot(&market, &[vec![42, 30]], &[vec![55, 20]]);

        let (yes_ask, no_ask, _, _) = market.kalshi.load();
        assert_eq!(no_ask, 58); // 100 - 42
        assert_eq!(yes_ask, 45); // 100 - 55

        let book = market.kalshi_book.lock();
        assert_eq!(book.yes_bid_count(), 1);
        assert!(!book.has_yes_bid(38));
    }
}

// ============================================================================
// KALSHI DELTA BUG PROOF - Demonstrates that order cancellation at the best
// bid level causes frozen (phantom) prices and sizes in the orderbook cache.
//
// This reproduces the root cause of the 2026-02-12 incident where MIL-ORL
// Total 220.5 was executed 5 times against phantom liquidity.
// See: https://github.com/Tydog99/Polymarket-Kalshi-Arbitrage-bot/issues/52
// ============================================================================

mod kalshi_delta_bug_proof {
    use arb_bot::types::*;
    use arb_bot::arb::ArbConfig;

    /// The exact logic from `process_kalshi_delta` in kalshi.rs:799-815,
    /// extracted here since the function is private.
    ///
    /// Given a delta's YES bid levels, compute the NO ask price and size.
    /// Falls back to (current_no, current_no_size) if no qualifying levels.
    fn compute_no_ask_from_yes_delta(
        levels: &[Vec<i64>],
        current_no: PriceCents,
        current_no_size: SizeCents,
    ) -> (PriceCents, SizeCents) {
        levels.iter()
            .filter_map(|l| {
                if l.len() >= 2 && l[1] > 0 {
                    Some((l[0], l[1]))
                } else {
                    None
                }
            })
            .max_by_key(|(p, _)| *p)
            .map(|(price, qty)| {
                let ask = (100 - price) as PriceCents;
                let size = (qty * price / 100) as SizeCents;
                (ask, size)
            })
            .unwrap_or((current_no, current_no_size))
    }

    /// PROOF: A delta cancelling the only bid level (qty=0) is silently ignored,
    /// preserving stale price and size.
    ///
    /// This is exactly what happened in the incident:
    /// 1. Snapshot sets NO ask = 64¢, size = 828¢ (from YES bid at 36¢)
    /// 2. Someone cancels their YES bid → delta: [36, 0]
    /// 3. Filter drops [36, 0] because qty=0
    /// 4. max_by_key returns None → unwrap_or preserves (64, 828)
    /// 5. Bot sees 828¢ of liquidity at 64¢ that no longer exists
    #[test]
    fn test_cancel_at_best_bid_preserves_stale_price_and_size() {
        // Step 1: Initial state from snapshot — YES bid at 36¢ with 23 contracts
        // NO ask = 100 - 36 = 64¢, size = 23 * 36 / 100 = 8 (828¢ with real values)
        let initial_no_ask: PriceCents = 64;
        let initial_no_size: SizeCents = 828;

        // Step 2: Delta arrives cancelling the bid: [36, 0]
        let cancel_delta = vec![vec![36_i64, 0_i64]];

        let (no_ask, no_size) = compute_no_ask_from_yes_delta(
            &cancel_delta,
            initial_no_ask,
            initial_no_size,
        );

        // BUG: Price and size are UNCHANGED despite the order being cancelled.
        // The bot will continue to see 828¢ of liquidity at 64¢.
        assert_eq!(no_ask, 64, "BUG: NO ask should be 0 (no bids), but delta handler preserved stale value");
        assert_eq!(no_size, 828, "BUG: NO size should be 0 (no liquidity), but delta handler preserved stale value");
    }

    /// PROOF: End-to-end through AtomicOrderbook — shows the stale cache persists
    /// and ArbOpportunity::detect() will find phantom arbs.
    #[test]
    fn test_phantom_arb_from_cancelled_kalshi_bid() {
        let kalshi_book = AtomicOrderbook::new();
        let poly_book = AtomicOrderbook::new();

        // Step 1: Kalshi has both sides priced (realistic: YES bid at 36¢, NO bid at 60¢)
        // YES ask = 100 - 60 = 40¢, NO ask = 100 - 36 = 64¢
        kalshi_book.store(40, 64, 600, 828);

        // Step 2: Poly has YES ask at 32¢ with size
        poly_book.store(32, 70, 2720, 500);

        // Step 3: Arb detection sees: Poly YES 32¢ + Kalshi NO 64¢ + fee ≈ 97¢ < 99¢
        let kalshi = kalshi_book.load();
        let poly = poly_book.load();
        let config = ArbConfig::default();
        let arb = ArbOpportunity::detect(0, kalshi, poly, &config, 0);
        assert!(arb.is_some(), "Arb should be detected with live liquidity");

        // Step 4: The YES bid at 36¢ gets cancelled → delta [36, 0].
        // Because of the bug, the delta handler falls back to (current_no, current_no_size).
        // Simulate: NO ask and NO size remain unchanged.
        let (current_yes, current_no, current_yes_size, current_no_size) = kalshi_book.load();
        assert_eq!(current_no, 64, "NO ask should still be 64¢");
        assert_eq!(current_no_size, 828, "NO size should still be 828¢");

        // The delta handler does exactly this: preserves stale values.
        kalshi_book.store(current_yes, current_no, current_yes_size, current_no_size);

        // Step 5: Arb detection STILL sees the phantom arb
        let kalshi_after = kalshi_book.load();
        let arb_after = ArbOpportunity::detect(0, kalshi_after, poly, &config, 0);
        assert!(arb_after.is_some(),
            "BUG: Arb is STILL detected after liquidity was cancelled — this is phantom liquidity");

        // What SHOULD happen: after the [36, 0] cancel delta, no_ask should become 0
        // (or the next-best bid level), and no_size should be 0. Then detect() returns None
        // because the arb no longer exists.
    }

    /// PROOF: Multiple deltas that only modify non-best levels leave the
    /// best-level size frozen indefinitely.
    #[test]
    fn test_non_best_level_deltas_leave_size_frozen() {
        // Initial: best YES bid at 36¢ x23, also a bid at 30¢ x5
        let initial_no_ask: PriceCents = 64;  // from 100 - 36
        let initial_no_size: SizeCents = 828; // from 23 * 36

        // Delta updates only the 30¢ level: [30, 10] (non-best bid)
        let delta = vec![vec![30_i64, 10_i64]];
        let (no_ask, no_size) = compute_no_ask_from_yes_delta(
            &delta,
            initial_no_ask,
            initial_no_size,
        );

        // The delta sees 30¢ as the best (only) bid in this message.
        // NO ask becomes 100 - 30 = 70¢, size = 10 * 30 / 100 = 3
        // This is wrong too — it overwrites the real best level (36¢)
        // with the delta's level (30¢), not knowing the 36¢ bid still exists.
        assert_eq!(no_ask, 70);
        assert_eq!(no_size, 3);
        // BUG: The delta handler has no memory of the full book — it can only
        // see levels in the current delta message, so a delta touching a
        // non-best level either gets ignored (if best is in the message)
        // or incorrectly replaces the best (if best is NOT in the message).
    }
}

// ============================================================================
// POLYMARKET SHADOW BOOK TESTS - Verifies PolyBook correctly tracks depth
// and feeds the AtomicOrderbook cache, preventing phantom prices.
// See: https://github.com/Tydog99/Polymarket-Kalshi-Arbitrage-bot/issues/57
// ============================================================================

mod poly_shadow_book_tests {
    use arb_bot::types::*;
    use arb_bot::types::{millis_to_cents, millis_size_to_cents};
    use arb_bot::arb::ArbConfig;

    /// Helper: create a GlobalState with one market pair registered.
    /// Returns (state, market_id).
    fn setup_state() -> (GlobalState, u16) {
        let state = GlobalState::default();
        let pair = MarketPair {
            pair_id: "test-poly-book".into(),
            league: "nba".into(),
            market_type: MarketType::Moneyline,
            description: "Test Poly Shadow Book".into(),
            kalshi_event_ticker: "KXNBAGAME-TEST".into(),
            kalshi_market_ticker: "KXNBAGAME-TEST-YES".into(),
            kalshi_event_slug: "test-event".into(),
            poly_slug: "test-poly".into(),
            poly_yes_token: "yes_token_poly_book".into(),
            poly_no_token: "no_token_poly_book".into(),
            line_value: None,
            team_suffix: None,
            neg_risk: false,
        };
        let id = state.add_pair(pair).expect("Should add pair");
        // Set Kalshi side so arb detection has complete data
        state.markets[id as usize].kalshi.store(50, 50, 1000, 1000);
        (state, id)
    }

    /// Snapshot populates PolyBook and AtomicOrderbook with correct best ask.
    #[test]
    fn test_poly_snapshot_populates_book_and_cache() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Simulate snapshot: YES token asks at 45¢, 48¢, 52¢ (in milli-cents)
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(480, 20000), (450, 15000), (520, 30000)]);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 45, "Best YES ask should be lowest: 45¢");
        assert_eq!(yes_size, 1500, "Size should be from 45¢ level");
    }

    /// Snapshot resets book after price_change deltas were applied.
    #[test]
    fn test_poly_snapshot_resets_after_deltas() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Initial snapshot
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(450, 10000)]);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }

        // Delta: add level at 42¢
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(420, 5000);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }
        assert_eq!(market.poly.load().0, 42, "Delta should have set best to 42¢");

        // New snapshot with completely different levels — replaces everything
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(600, 8000), (650, 12000)]);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }
        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 60, "Snapshot should fully replace: best=60¢");
        assert_eq!(yes_size, 800, "Size from new snapshot's best level");
    }

    /// price_change with size updates both PolyBook and AtomicOrderbook.
    #[test]
    fn test_poly_price_change_updates_book() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Initial snapshot: YES ask at 50¢
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(500, 10000)]);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }

        // price_change: new SELL level at 48¢ with size 500
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(480, 5000);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 48, "New lower ask should become best");
        assert_eq!(yes_size, 500, "Size should be from new level");
    }

    /// Size from price_change is used, not stale snapshot size.
    #[test]
    fn test_poly_size_updates_from_price_change() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot: YES ask at 45¢ with size 1000
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(450, 10000)]);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }
        assert_eq!(market.poly.load().2, 1000);

        // price_change: same price 45¢ but size updated to 2500
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(450, 25000);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 45, "Price unchanged");
        assert_eq!(yes_size, 2500, "Size should be updated from price_change, not stale");
    }

    /// Removing best ask via size=0 reveals next-best level.
    #[test]
    fn test_poly_best_ask_removed_reveals_next() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot: asks at 45¢ and 50¢
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(450, 10000), (500, 20000)]);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }
        assert_eq!(market.poly.load().0, 45);

        // price_change: remove 45¢ level (size=0)
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(450, 0);
            let best = book.best_yes_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_yes(millis_to_cents(best.0), millis_size_to_cents(best.1));
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 50, "Next-best level (50¢) should be promoted");
        assert_eq!(yes_size, 2000, "Size from 50¢ level");
    }

    /// End-to-end: arb exists, then best ask is removed, arb disappears.
    #[test]
    fn test_poly_no_phantom_arb_after_removal() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];
        let config = ArbConfig::new(99, 1.0);

        // Kalshi: YES ask = 50¢, NO ask = 50¢
        market.kalshi.store(50, 50, 1000, 1000);

        // Poly: YES ask = 45¢, NO ask = 55¢ (need non-zero for detect())
        // PolyYesKalshiNo cost = 45 + 50 + kalshi_fee(50) = 45 + 50 + 2 = 97 < 99 → arb exists
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(450, 10000)]);
            book.set_no_asks(&[(550, 10000)]);
            let (p, s) = book.best_yes_ask().unwrap();
            let (np, ns) = book.best_no_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p), millis_size_to_cents(s));
            market.poly.update_no(millis_to_cents(np), millis_size_to_cents(ns));
        }

        let arb = ArbOpportunity::detect(
            id, market.kalshi.load(), market.poly.load(), &config, 0,
        );
        assert!(arb.is_some(), "Arb should exist: 45 + 50 + fee(2) = 97 < 99");

        // Now remove the 45¢ level — no other levels exist
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(450, 0);
            let best = book.best_yes_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_yes(millis_to_cents(best.0), millis_size_to_cents(best.1));
        }

        let arb = ArbOpportunity::detect(
            id, market.kalshi.load(), market.poly.load(), &config, 0,
        );
        assert!(arb.is_none(), "Arb should NOT exist: price=0 means no liquidity");
    }

    /// All levels removed → cache shows (0, 0) → arb detection skips.
    #[test]
    fn test_poly_empty_book_clears_cache() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot with two levels
        {
            let mut book = market.poly_book.lock();
            book.set_yes_asks(&[(450, 10000), (500, 20000)]);
            let (p_millis, s_millis) = book.best_yes_ask().unwrap();
            drop(book);
            market.poly.update_yes(millis_to_cents(p_millis), millis_size_to_cents(s_millis));
        }

        // Remove both levels
        {
            let mut book = market.poly_book.lock();
            book.update_yes_level(450, 0);
            book.update_yes_level(500, 0);
            let best = book.best_yes_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_yes(millis_to_cents(best.0), millis_size_to_cents(best.1));
        }

        let (yes_ask, _, yes_size, _) = market.poly.load();
        assert_eq!(yes_ask, 0, "Empty book → price 0");
        assert_eq!(yes_size, 0, "Empty book → size 0");
    }

    /// Regression: sub-cent prices that previously collided at same cent value.
    /// API sends 0.999, 0.998, 0.99 — all mapped to 99¢ before fix.
    #[test]
    fn test_poly_subcent_no_collision() {
        let (state, id) = setup_state();
        let market = &state.markets[id as usize];

        // Snapshot with sub-cent prices near 97¢
        {
            let mut book = market.poly_book.lock();
            book.set_no_asks(&[(970, 10000), (978, 5000), (979, 12900)]);
            let (p, s) = book.best_no_ask().unwrap();
            drop(book);
            market.poly.update_no(millis_to_cents(p), millis_size_to_cents(s));
        }
        assert_eq!(market.poly.load().1, 97); // 970 millis → 97¢

        // Remove 0.97 level — 0.978 should become best (not empty!)
        {
            let mut book = market.poly_book.lock();
            book.update_no_level(970, 0);
            let best = book.best_no_ask().unwrap_or((0, 0));
            drop(book);
            market.poly.update_no(millis_to_cents(best.0), millis_size_to_cents(best.1));
        }
        // Before fix: NO ask would jump to 0 or 99 (lost depth). After fix: 97¢ from 0.978
        assert_eq!(market.poly.load().1, 97, "Sub-cent level at 0.978 should still show as 97¢");
    }
}