//! Integration tests for hybrid execution.
//!
//! These tests verify the HybridExecutor works correctly in various scenarios:
//! 1. Local execution only (platform in local_platforms, no remote)
//! 2. Remote routing (remote trader connected)
//! 3. Hybrid mode (some legs local, some remote)
//! 4. Drop behavior (neither local nor remote available)

use arb_bot::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use arb_bot::remote_execution::HybridExecutor;
use arb_bot::remote_protocol::{IncomingMessage as WsMsg, Platform as WsPlatform};
use arb_bot::remote_trader::RemoteTraderServer;
use arb_bot::types::{ArbType, ArbOpportunity, GlobalState, MarketPair, MarketType};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use trading::execution::Platform as TradingPlatform;

// =============================================================================
// TEST HELPERS
// =============================================================================

fn make_test_pair() -> MarketPair {
    MarketPair {
        pair_id: "hybrid-test-pair".into(),
        league: "epl".into(),
        market_type: MarketType::Moneyline,
        description: "Hybrid Test Market".into(),
        kalshi_event_ticker: "KXHYBRID".into(),
        kalshi_market_ticker: "KXHYBRID-MKT".into(),
        kalshi_event_slug: "hybrid-test".into(),
        poly_slug: "hybrid-test".into(),
        poly_yes_token: "0xhybrid_yes".into(),
        poly_no_token: "0xhybrid_no".into(),
        line_value: None,
        team_suffix: None,
        neg_risk: false,
    }
}

fn cb_disabled() -> Arc<CircuitBreaker> {
    Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
        max_position_per_market: 1_000_000.0,
        max_total_position: 1_000_000.0,
        max_daily_loss: 1_000_000.0,
        max_consecutive_errors: 999,
        cooldown_secs: 0,
        enabled: false,
        min_contracts: 1,
    }))
}

fn make_state_with_pair(pair: MarketPair) -> Arc<GlobalState> {
    let state = Arc::new(GlobalState::default());
    let id = state.add_pair(pair).expect("add_pair");
    assert_eq!(id, 0);
    state
}

fn make_arb_request(arb_type: ArbType) -> ArbOpportunity {
    ArbOpportunity {
        market_id: 0,
        yes_price: 40,
        no_price: 50,
        yes_size: 1000, // 10 contracts
        no_size: 1000,
        arb_type,
        detected_ns: 0,
        is_test: false,
    }
}

// =============================================================================
// TEST: LOCAL EXECUTION ONLY (NO REMOTE TRADERS)
// =============================================================================

/// When local_platforms = {Kalshi} and no remote traders, a PolyYesKalshiNo arb
/// should be DROPPED because there's no way to execute the Poly leg.
#[tokio::test]
async fn test_hybrid_executor_local_kalshi_drops_poly_arb() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    // Create server but don't register any traders
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    // Only Kalshi is authorized locally - but no Kalshi remote trader either
    // This means we CAN execute Kalshi locally but CANNOT execute Poly at all
    let mut local_platforms = HashSet::new();
    local_platforms.insert(TradingPlatform::Kalshi);

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None, // No Kalshi API client (but we're in dry_run mode)
        None, // No Poly client
        true, // dry_run
    );

    // Try to execute a cross-platform arb (Poly YES + Kalshi NO)
    let req = make_arb_request(ArbType::PolyYesKalshiNo);

    // Process should complete without error, but silently drop the arb
    // because we can't execute the Poly leg
    let result = exec.process(req).await;
    assert!(result.is_ok(), "process should not error, just drop");

    // Verify the arb was indeed dropped by checking that:
    // - No messages were sent to any trader (none are connected)
    // - The process returned Ok (didn't panic or error)
    // This test mainly ensures the code handles this gracefully
}

/// When local_platforms = {Polymarket} and no remote traders, a PolyYesKalshiNo arb
/// should be DROPPED because there's no way to execute the Kalshi leg.
#[tokio::test]
async fn test_hybrid_executor_local_poly_drops_kalshi_arb() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    // Only Polymarket is authorized locally
    let mut local_platforms = HashSet::new();
    local_platforms.insert(TradingPlatform::Polymarket);

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    // Try to execute a cross-platform arb
    let req = make_arb_request(ArbType::PolyYesKalshiNo);
    let result = exec.process(req).await;
    assert!(result.is_ok(), "process should not error, just drop");
}

/// When local_platforms is EMPTY and no remote traders, ALL arbs should be dropped.
#[tokio::test]
async fn test_hybrid_executor_no_local_no_remote_drops_all() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    // No local platforms authorized
    let local_platforms = HashSet::new();

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    // All arb types should be dropped
    for arb_type in [
        ArbType::PolyYesKalshiNo,
        ArbType::KalshiYesPolyNo,
        ArbType::PolyOnly,
        ArbType::KalshiOnly,
    ] {
        let req = make_arb_request(arb_type);
        let result = exec.process(req).await;
        assert!(result.is_ok(), "process should not error for {:?}", arb_type);
    }
}

// =============================================================================
// TEST: REMOTE ROUTING
// =============================================================================

/// When a remote Poly trader is connected, the Poly leg should be routed to it.
/// Kalshi leg should also go to remote if Kalshi trader is connected.
#[tokio::test]
async fn test_hybrid_executor_routes_to_remote_when_available() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    // Register both remote traders
    let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;
    let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

    // No local platforms - all execution goes remote
    let local_platforms = HashSet::new();

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    let req = make_arb_request(ArbType::PolyYesKalshiNo);
    exec.process(req).await.unwrap();

    // Both traders should receive their respective legs
    let kalshi_msg = kalshi_rx.try_recv().expect("kalshi should receive leg");
    let poly_msg = poly_rx.try_recv().expect("poly should receive leg");

    // Verify Kalshi leg
    match kalshi_msg {
        WsMsg::ExecuteLeg { platform, side, kalshi_market_ticker, poly_token, .. } => {
            assert_eq!(platform, WsPlatform::Kalshi);
            assert!(kalshi_market_ticker.is_some(), "Kalshi leg should have ticker");
            assert!(poly_token.is_none(), "Kalshi leg should not have poly token");
            // For PolyYesKalshiNo, Kalshi gets the NO side
            assert_eq!(side, arb_bot::remote_protocol::OutcomeSide::No);
        }
        other => panic!("unexpected message to kalshi: {:?}", other),
    }

    // Verify Poly leg
    match poly_msg {
        WsMsg::ExecuteLeg { platform, side, kalshi_market_ticker, poly_token, .. } => {
            assert_eq!(platform, WsPlatform::Polymarket);
            assert!(kalshi_market_ticker.is_none(), "Poly leg should not have kalshi ticker");
            assert!(poly_token.is_some(), "Poly leg should have token");
            // For PolyYesKalshiNo, Poly gets the YES side
            assert_eq!(side, arb_bot::remote_protocol::OutcomeSide::Yes);
        }
        other => panic!("unexpected message to poly: {:?}", other),
    }
}

/// When only one remote trader is connected, arbs requiring both should be dropped.
#[tokio::test]
async fn test_hybrid_executor_drops_arb_when_missing_remote_trader() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    // Only Kalshi trader connected
    let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;

    // No local platforms
    let local_platforms = HashSet::new();

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    // Cross-platform arb should be dropped (no Poly trader)
    let req = make_arb_request(ArbType::PolyYesKalshiNo);
    exec.process(req).await.unwrap();

    // Kalshi should NOT receive anything because the whole arb was dropped
    assert!(kalshi_rx.try_recv().is_err(), "should not send kalshi leg when poly is missing");
}

// =============================================================================
// TEST: HYBRID MODE (SOME LOCAL, SOME REMOTE)
// =============================================================================

/// When Kalshi is in local_platforms and Poly trader is connected remotely,
/// a PolyYesKalshiNo arb should:
/// - Execute Kalshi leg locally (or dry_run log it)
/// - Route Poly leg to remote trader
#[tokio::test]
async fn test_hybrid_executor_local_kalshi_remote_poly() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    // Only Poly trader connected remotely
    let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

    // Kalshi authorized locally
    let mut local_platforms = HashSet::new();
    local_platforms.insert(TradingPlatform::Kalshi);

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None, // No actual Kalshi client (dry_run will handle it)
        None,
        true, // dry_run
    );

    let req = make_arb_request(ArbType::PolyYesKalshiNo);
    exec.process(req).await.unwrap();

    // Poly leg should be sent to remote trader
    let poly_msg = poly_rx.try_recv().expect("poly should receive leg");
    match poly_msg {
        WsMsg::ExecuteLeg { platform, .. } => {
            assert_eq!(platform, WsPlatform::Polymarket);
        }
        other => panic!("unexpected message: {:?}", other),
    }

    // Note: Kalshi leg executed locally (in dry_run mode, just logs)
    // We can't directly observe this without mocking the local execution
}

/// When Poly is in local_platforms and Kalshi trader is connected remotely,
/// the reverse hybrid mode should work.
#[tokio::test]
async fn test_hybrid_executor_local_poly_remote_kalshi() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    // Only Kalshi trader connected remotely
    let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;

    // Poly authorized locally
    let mut local_platforms = HashSet::new();
    local_platforms.insert(TradingPlatform::Polymarket);

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    let req = make_arb_request(ArbType::PolyYesKalshiNo);
    exec.process(req).await.unwrap();

    // Kalshi leg should be sent to remote trader
    let kalshi_msg = kalshi_rx.try_recv().expect("kalshi should receive leg");
    match kalshi_msg {
        WsMsg::ExecuteLeg { platform, .. } => {
            assert_eq!(platform, WsPlatform::Kalshi);
        }
        other => panic!("unexpected message: {:?}", other),
    }
}

// =============================================================================
// TEST: SAME-PLATFORM ARBS
// =============================================================================

/// PolyOnly arb should work when Poly trader is connected.
#[tokio::test]
async fn test_hybrid_executor_poly_only_arb() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Polymarket], true);
    let router = server.router();

    let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

    let local_platforms = HashSet::new();

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    let req = make_arb_request(ArbType::PolyOnly);
    exec.process(req).await.unwrap();

    // Both legs should go to Poly trader
    let msg1 = poly_rx.try_recv().expect("first poly leg");
    let msg2 = poly_rx.try_recv().expect("second poly leg");

    match (msg1, msg2) {
        (WsMsg::ExecuteLeg { platform: p1, side: s1, .. }, WsMsg::ExecuteLeg { platform: p2, side: s2, .. }) => {
            assert_eq!(p1, WsPlatform::Polymarket);
            assert_eq!(p2, WsPlatform::Polymarket);
            // Should have both YES and NO sides
            let sides = vec![s1, s2];
            assert!(sides.contains(&arb_bot::remote_protocol::OutcomeSide::Yes));
            assert!(sides.contains(&arb_bot::remote_protocol::OutcomeSide::No));
        }
        (other1, other2) => panic!("unexpected messages: {:?}, {:?}", other1, other2),
    }
}

/// KalshiOnly arb should work when Kalshi trader is connected.
#[tokio::test]
async fn test_hybrid_executor_kalshi_only_arb() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi], true);
    let router = server.router();

    let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;

    let local_platforms = HashSet::new();

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    let req = make_arb_request(ArbType::KalshiOnly);
    exec.process(req).await.unwrap();

    // Both legs should go to Kalshi trader
    let msg1 = kalshi_rx.try_recv().expect("first kalshi leg");
    let msg2 = kalshi_rx.try_recv().expect("second kalshi leg");

    match (msg1, msg2) {
        (WsMsg::ExecuteLeg { platform: p1, side: s1, .. }, WsMsg::ExecuteLeg { platform: p2, side: s2, .. }) => {
            assert_eq!(p1, WsPlatform::Kalshi);
            assert_eq!(p2, WsPlatform::Kalshi);
            // Should have both YES and NO sides
            let sides = vec![s1, s2];
            assert!(sides.contains(&arb_bot::remote_protocol::OutcomeSide::Yes));
            assert!(sides.contains(&arb_bot::remote_protocol::OutcomeSide::No));
        }
        (other1, other2) => panic!("unexpected messages: {:?}, {:?}", other1, other2),
    }
}

// =============================================================================
// TEST: PROFIT THRESHOLD AND SIZE CHECKS
// =============================================================================

/// Arbs with zero profit should be dropped.
#[tokio::test]
async fn test_hybrid_executor_drops_zero_profit_arb() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;
    let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

    let local_platforms = HashSet::new();

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    // Prices sum to more than $1 (no profit)
    let req = ArbOpportunity {
        market_id: 0,
        yes_price: 55,
        no_price: 55, // 55 + 55 + fees > 100
        yes_size: 1000,
        no_size: 1000,
        arb_type: ArbType::PolyYesKalshiNo,
        detected_ns: 0,
        is_test: false,
    };

    exec.process(req).await.unwrap();

    // No messages should be sent
    assert!(kalshi_rx.try_recv().is_err(), "no kalshi message expected");
    assert!(poly_rx.try_recv().is_err(), "no poly message expected");
}

/// Arbs with insufficient size should be dropped.
#[tokio::test]
async fn test_hybrid_executor_drops_insufficient_size_arb() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;
    let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

    let local_platforms = HashSet::new();

    let exec = HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    );

    // Size too small for even 1 contract
    // max_contracts = min(yes_size/yes_price, no_size/no_price) = min(39/40, 49/50) = min(0, 0) = 0
    let req = ArbOpportunity {
        market_id: 0,
        yes_price: 40,
        no_price: 50,
        yes_size: 39,  // 39/40 = 0 contracts
        no_size: 49,   // 49/50 = 0 contracts
        arb_type: ArbType::PolyYesKalshiNo,
        detected_ns: 0,
        is_test: false,
    };

    exec.process(req).await.unwrap();

    // No messages should be sent
    assert!(kalshi_rx.try_recv().is_err(), "no kalshi message expected");
    assert!(poly_rx.try_recv().is_err(), "no poly message expected");
}

// =============================================================================
// TEST: DEDUPLICATION
// =============================================================================

/// The same market_id should only be processed once (in-flight deduplication).
#[tokio::test]
async fn test_hybrid_executor_deduplication() {
    let state = make_state_with_pair(make_test_pair());
    let cb = cb_disabled();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
    let router = server.router();

    let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;
    let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

    let local_platforms = HashSet::new();

    let exec = Arc::new(HybridExecutor::new(
        state,
        cb,
        router,
        local_platforms,
        None,
        None,
        true,
    ));

    let req = make_arb_request(ArbType::PolyYesKalshiNo);

    // Process the same request twice immediately
    let exec1 = exec.clone();
    let exec2 = exec.clone();

    let h1 = tokio::spawn(async move {
        exec1.process(req.clone()).await
    });

    // Small delay to ensure ordering
    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

    let h2 = tokio::spawn(async move {
        let req2 = ArbOpportunity {
            market_id: 0, // Same market_id
            ..req
        };
        exec2.process(req2).await
    });

    h1.await.unwrap().unwrap();
    h2.await.unwrap().unwrap();

    // First request should have sent messages
    let kalshi_msg = kalshi_rx.try_recv();
    let poly_msg = poly_rx.try_recv();
    assert!(kalshi_msg.is_ok() || poly_msg.is_ok(), "at least one message expected from first request");

    // Second request should have been deduplicated (no additional messages)
    // Note: There might be 2 messages from first request (one to each platform)
    // So we check that there are at most 2 messages total, not 4
    let total_messages = kalshi_rx.try_recv().ok().map(|_| 1).unwrap_or(0)
        + poly_rx.try_recv().ok().map(|_| 1).unwrap_or(0);

    // After consuming the initial messages, there should be no more
    // (the second request was deduplicated)
    assert!(total_messages <= 1, "deduplication should prevent second request from sending");
}
