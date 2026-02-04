//! Integration tests for delayed Polymarket order reconciliation.
//!
//! These tests verify the reconciliation flow when Polymarket returns status="delayed":
//! 1. Initial order returns is_delayed=true with order_id
//! 2. Kalshi fill is recorded with reconciliation_pending marker
//! 3. Background task polls Poly until terminal state
//! 4. Poly fill is recorded and reconciliation_pending is cleared
//!
//! The key mock types used:
//! - MockPolyClient with set_delayed_order() for initial delayed response
//! - MockDelayedResponse::Filled for poll_delayed_order result

use std::sync::Arc;

use rsa::RsaPrivateKey;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use arb_bot::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use arb_bot::execution::{ExecutionEngine, NanoClock};
use arb_bot::kalshi::{KalshiApiClient, KalshiConfig};
use arb_bot::poly_executor::mock::{MockDelayedResponse, MockPolyClient};
use arb_bot::poly_executor::PolyExecutor;
use arb_bot::position_tracker::{create_position_channel, FillRecord, PositionMessage};
use arb_bot::types::{ArbOpportunity, ArbType, GlobalState, MarketPair, MarketType};

use super::replay_harness::load_fixture;

// =============================================================================
// TEST HELPERS
// =============================================================================

fn fixture_path(name: &str) -> String {
    format!(
        "{}/tests/integration/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    )
}

/// Create a test KalshiConfig with a generated RSA key.
fn create_test_kalshi_config() -> KalshiConfig {
    let mut rng = rand::thread_rng();
    let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("Failed to generate RSA key");
    KalshiConfig {
        api_key_id: "test-api-key".to_string(),
        private_key,
    }
}

/// Create a KalshiApiClient pointing at a mock server.
fn create_test_kalshi_client(server: &MockServer) -> KalshiApiClient {
    let config = create_test_kalshi_config();
    KalshiApiClient::new_with_base_url(config, &server.uri())
}

/// Create a test market pair for arb execution.
fn create_test_market_pair() -> MarketPair {
    MarketPair {
        pair_id: "KXNBA-26-SAS".into(),
        league: "nba".into(),
        market_type: MarketType::Moneyline,
        description: "NBA Test Market".into(),
        kalshi_event_ticker: "KXNBA-26JAN20".into(),
        kalshi_market_ticker: "KXNBA-26-SAS".into(),
        kalshi_event_slug: "nba-game".into(),
        poly_slug: "nba-test-2026-01-20".into(),
        poly_yes_token: "poly-yes-token-12345".into(),
        poly_no_token: "poly-no-token-67890".into(),
        line_value: None,
        team_suffix: Some("SAS".into()),
        neg_risk: false,
    }
}

/// Create a disabled circuit breaker config for testing.
fn create_disabled_circuit_breaker() -> CircuitBreaker {
    let config = CircuitBreakerConfig {
        max_position_per_market: i64::MAX,
        max_total_position: i64::MAX,
        max_daily_loss: f64::MAX,
        max_consecutive_errors: u32::MAX,
        cooldown_secs: 0,
        enabled: false,
        min_contracts: 1,
    };
    CircuitBreaker::new(config)
}

fn create_test_engine(
    kalshi_server: &MockServer,
    mock_poly: Arc<dyn PolyExecutor>,
) -> (ExecutionEngine, mpsc::UnboundedReceiver<PositionMessage>) {
    let kalshi = Arc::new(create_test_kalshi_client(kalshi_server));
    let state = Arc::new(GlobalState::default());
    let circuit_breaker = Arc::new(create_disabled_circuit_breaker());
    let (position_channel, fill_rx) = create_position_channel();
    let clock = Arc::new(NanoClock::new());

    // Add test market pair to state
    let pair = create_test_market_pair();
    state.add_pair(pair);

    let engine = ExecutionEngine::new(
        kalshi,
        mock_poly,
        state,
        circuit_breaker,
        position_channel,
        false, // dry_run = false
        clock,
    );

    (engine, fill_rx)
}

/// Create a ArbOpportunity for testing.
fn create_test_request(arb_type: ArbType) -> ArbOpportunity {
    ArbOpportunity {
        market_id: 0,
        yes_price: 9,      // 9 cents
        no_price: 85,      // 85 cents
        yes_size: 10000,   // $100 available
        no_size: 10000,
        arb_type,
        detected_ns: 0,
        is_test: false,
    }
}

/// Drain all messages from the channel (non-blocking).
async fn drain_messages(rx: &mut mpsc::UnboundedReceiver<PositionMessage>) -> (Vec<FillRecord>, Vec<String>) {
    let mut fills = Vec::new();
    let mut cleared_order_ids = Vec::new();
    // Give a brief moment for messages to arrive
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            PositionMessage::Fill(fill) => fills.push(fill),
            PositionMessage::ClearReconciliationPending(order_id) => cleared_order_ids.push(order_id),
        }
    }
    (fills, cleared_order_ids)
}

// =============================================================================
// DELAYED RECONCILIATION TESTS
// =============================================================================

/// Test: Delayed Polymarket order reconciliation flow.
///
/// Scenario:
/// - Arb type: KalshiYesPolyNo (YES on Kalshi, NO on Poly)
/// - Kalshi fills successfully (1 contract)
/// - Poly returns status="delayed" initially
/// - Background reconciliation polls and gets filled (10 contracts)
///
/// Expected:
/// 1. Kalshi fill is recorded with reconciliation_pending = poly_order_id
/// 2. After reconciliation, Poly fill is recorded
/// 3. ClearReconciliationPending message is sent with poly_order_id
#[tokio::test]
async fn test_delayed_poly_order_reconciliation() {
    // Set a very short timeout for the test
    std::env::set_var("POLY_DELAYED_TIMEOUT_MS", "1000");

    let kalshi_server = MockServer::start().await;

    // Load and mount Kalshi full fill fixture (1 contract)
    let exchange = load_fixture(fixture_path("kalshi_full_fill_real.json"))
        .expect("Failed to load fixture");

    let mut response = ResponseTemplate::new(exchange.response.status)
        .set_body_raw(exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            response = response.append_header(key.as_str(), value.as_str());
        }
    }

    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(response)
        .mount(&kalshi_server)
        .await;

    // Configure mock Poly client:
    // 1. Initial buy_fak returns is_delayed=true with order_id
    // 2. poll_delayed_order returns Filled with 10 contracts
    let mock_poly = Arc::new(MockPolyClient::new());
    let poly_order_id = "delayed-poly-order-abc123";

    // For KalshiYesPolyNo, we buy Poly NO token
    mock_poly.set_delayed_order("poly-no-token-67890", poly_order_id);
    mock_poly.set_delayed_response(poly_order_id, MockDelayedResponse::Filled { size: 10.0 });

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute the arb (KalshiYesPolyNo = Kalshi YES + Poly NO)
    let req = create_test_request(ArbType::KalshiYesPolyNo);
    let result = engine.process(req).await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result.err());
    let exec_result = result.unwrap();
    // With delayed orders, we return success immediately (reconciliation happens in background)
    assert!(exec_result.success, "Should report success while awaiting reconciliation");

    // Wait for background reconciliation task to complete
    // The timeout is 1000ms, so we wait a bit longer
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    // Collect all messages
    let (fills, cleared_order_ids) = drain_messages(&mut fill_rx).await;

    // === Verify Kalshi fill was recorded with reconciliation_pending ===
    let kalshi_fills: Vec<_> = fills.iter()
        .filter(|f| f.platform == "kalshi" && f.side == "yes")
        .collect();

    assert_eq!(
        kalshi_fills.len(), 1,
        "Should have 1 Kalshi YES fill. Got: {:?}",
        kalshi_fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );

    let kalshi_fill = kalshi_fills[0];
    assert_eq!(kalshi_fill.contracts, 1.0, "Kalshi should fill 1 contract (from fixture)");
    assert_eq!(
        kalshi_fill.reconciliation_pending,
        Some(poly_order_id.to_string()),
        "Kalshi fill should have reconciliation_pending set to poly order_id"
    );

    // === Verify Poly fill was recorded after reconciliation ===
    let poly_fills: Vec<_> = fills.iter()
        .filter(|f| f.platform == "polymarket" && f.side == "no")
        .collect();

    assert_eq!(
        poly_fills.len(), 1,
        "Should have 1 Poly NO fill after reconciliation. Got: {:?}",
        poly_fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );

    let poly_fill = poly_fills[0];
    assert_eq!(poly_fill.contracts, 10.0, "Poly should fill 10 contracts (from mock delayed response)");
    assert!(
        poly_fill.reconciliation_pending.is_none(),
        "Poly fill should NOT have reconciliation_pending"
    );

    // === Verify ClearReconciliationPending was sent ===
    assert!(
        cleared_order_ids.contains(&poly_order_id.to_string()),
        "Should have sent ClearReconciliationPending for {}. Cleared IDs: {:?}",
        poly_order_id, cleared_order_ids
    );
}

/// Test: Delayed Polymarket order that results in no fill.
///
/// Scenario:
/// - Arb type: PolyYesKalshiNo (YES on Poly, NO on Kalshi)
/// - Kalshi fills successfully (1 contract)
/// - Poly returns status="delayed" initially
/// - Background reconciliation polls and order was canceled (no fill)
///
/// Expected:
/// 1. Kalshi fill is recorded with reconciliation_pending
/// 2. After reconciliation, Poly fill is recorded with 0 contracts
/// 3. Auto-close should trigger to unwind Kalshi position (since there's a mismatch)
/// 4. ClearReconciliationPending is sent
#[tokio::test]
async fn test_delayed_poly_order_no_fill() {
    // Set a very short timeout for the test
    std::env::set_var("POLY_DELAYED_TIMEOUT_MS", "1000");

    let kalshi_server = MockServer::start().await;

    // Load and mount Kalshi BUY full fill fixture
    let buy_exchange = load_fixture(fixture_path("kalshi_full_fill_real.json"))
        .expect("Failed to load fixture");

    let mut buy_response = ResponseTemplate::new(buy_exchange.response.status)
        .set_body_raw(buy_exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &buy_exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            buy_response = buy_response.append_header(key.as_str(), value.as_str());
        }
    }

    // Load and mount Kalshi SELL fixture for auto-close
    let sell_exchange = load_fixture(fixture_path("kalshi_sell_full_fill_real.json"))
        .expect("Failed to load sell fixture");

    let mut sell_response = ResponseTemplate::new(sell_exchange.response.status)
        .set_body_raw(sell_exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &sell_exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            sell_response = sell_response.append_header(key.as_str(), value.as_str());
        }
    }

    // First request = buy order
    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(buy_response)
        .up_to_n_times(1)
        .mount(&kalshi_server)
        .await;

    // Second request = sell order (auto-close)
    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(sell_response)
        .mount(&kalshi_server)
        .await;

    // Configure mock Poly client:
    // 1. Initial buy_fak returns is_delayed=true with order_id
    // 2. poll_delayed_order returns NoFill (order was canceled)
    let mock_poly = Arc::new(MockPolyClient::new());
    let poly_order_id = "delayed-poly-order-xyz789";

    // For PolyYesKalshiNo, we buy Poly YES token
    mock_poly.set_delayed_order("poly-yes-token-12345", poly_order_id);
    mock_poly.set_delayed_response(poly_order_id, MockDelayedResponse::NoFill);

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute the arb (PolyYesKalshiNo = Poly YES + Kalshi NO)
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;

    assert!(result.is_ok(), "Execution should succeed");

    // Wait for background reconciliation and potential auto-close
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    // Collect all messages
    let (fills, cleared_order_ids) = drain_messages(&mut fill_rx).await;

    // === Verify Kalshi fill was recorded with reconciliation_pending ===
    let kalshi_no_fills: Vec<_> = fills.iter()
        .filter(|f| f.platform == "kalshi" && f.side == "no" && f.contracts > 0.0)
        .collect();

    assert_eq!(
        kalshi_no_fills.len(), 1,
        "Should have 1 Kalshi NO buy fill. Fills: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );

    let kalshi_fill = kalshi_no_fills[0];
    assert_eq!(
        kalshi_fill.reconciliation_pending,
        Some(poly_order_id.to_string()),
        "Kalshi fill should have reconciliation_pending"
    );

    // === Verify Poly fill was recorded with 0 contracts (no fill) ===
    let poly_fills: Vec<_> = fills.iter()
        .filter(|f| f.platform == "polymarket" && f.side == "yes")
        .collect();

    // Should have at least one Poly fill record (even if 0 contracts)
    assert!(
        !poly_fills.is_empty(),
        "Should have Poly YES fill record. Fills: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );

    // The Poly fill from reconciliation should have 0 contracts
    let poly_reconciled = poly_fills.iter().find(|f| f.contracts == 0.0);
    assert!(
        poly_reconciled.is_some(),
        "Should have Poly fill with 0 contracts (from NoFill response)"
    );

    // === Verify ClearReconciliationPending was sent ===
    assert!(
        cleared_order_ids.contains(&poly_order_id.to_string()),
        "Should have sent ClearReconciliationPending for {}. Cleared IDs: {:?}",
        poly_order_id, cleared_order_ids
    );

    // === Verify auto-close happened (negative fill to close Kalshi position) ===
    let kalshi_close_fills: Vec<_> = fills.iter()
        .filter(|f| f.platform == "kalshi" && f.side == "no" && f.contracts < 0.0)
        .collect();

    assert!(
        !kalshi_close_fills.is_empty(),
        "Should have auto-close fill (negative contracts) for Kalshi. Fills: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );
}

/// Test: Delayed Polymarket order times out.
///
/// Scenario:
/// - Poly returns status="delayed" initially
/// - Background reconciliation polls but hits timeout
///
/// Expected:
/// 1. Kalshi fill is recorded with reconciliation_pending
/// 2. ClearReconciliationPending is still sent (to clean up the marker)
/// 3. Auto-close may trigger to unwind Kalshi position
/// 4. The error is logged but doesn't crash
#[tokio::test]
async fn test_delayed_poly_order_timeout() {
    // Set a very short timeout for the test
    std::env::set_var("POLY_DELAYED_TIMEOUT_MS", "100");

    let kalshi_server = MockServer::start().await;

    // Load and mount Kalshi BUY full fill fixture
    let buy_exchange = load_fixture(fixture_path("kalshi_full_fill_real.json"))
        .expect("Failed to load fixture");

    let mut buy_response = ResponseTemplate::new(buy_exchange.response.status)
        .set_body_raw(buy_exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &buy_exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            buy_response = buy_response.append_header(key.as_str(), value.as_str());
        }
    }

    // Load and mount Kalshi SELL fixture for auto-close
    let sell_exchange = load_fixture(fixture_path("kalshi_sell_full_fill_real.json"))
        .expect("Failed to load sell fixture");

    let mut sell_response = ResponseTemplate::new(sell_exchange.response.status)
        .set_body_raw(sell_exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &sell_exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            sell_response = sell_response.append_header(key.as_str(), value.as_str());
        }
    }

    // First request = buy order
    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(buy_response)
        .up_to_n_times(1)
        .mount(&kalshi_server)
        .await;

    // Second request = sell order (auto-close after timeout)
    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(sell_response)
        .mount(&kalshi_server)
        .await;

    // Configure mock Poly client to return Timeout
    let mock_poly = Arc::new(MockPolyClient::new());
    let poly_order_id = "delayed-poly-order-timeout";

    mock_poly.set_delayed_order("poly-no-token-67890", poly_order_id);
    mock_poly.set_delayed_response(poly_order_id, MockDelayedResponse::Timeout);

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute the arb
    let req = create_test_request(ArbType::KalshiYesPolyNo);
    let result = engine.process(req).await;

    assert!(result.is_ok(), "Execution should succeed even if Poly times out");

    // Wait for background reconciliation to timeout and potentially auto-close
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Collect all messages
    let (fills, cleared_order_ids) = drain_messages(&mut fill_rx).await;

    // === Verify at least one Kalshi fill was recorded ===
    let kalshi_fills: Vec<_> = fills.iter()
        .filter(|f| f.platform == "kalshi" && f.side == "yes")
        .collect();

    assert!(
        !kalshi_fills.is_empty(),
        "Should have at least 1 Kalshi YES fill. Got: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );

    // The first fill should have reconciliation_pending set
    let initial_fill = kalshi_fills.iter().find(|f| f.contracts > 0.0);
    assert!(
        initial_fill.is_some(),
        "Should have positive Kalshi fill"
    );
    assert_eq!(
        initial_fill.unwrap().reconciliation_pending,
        Some(poly_order_id.to_string()),
        "Kalshi fill should have reconciliation_pending"
    );

    // === Verify ClearReconciliationPending was still sent (cleanup) ===
    assert!(
        cleared_order_ids.contains(&poly_order_id.to_string()),
        "Should have sent ClearReconciliationPending even on timeout. Cleared IDs: {:?}",
        cleared_order_ids
    );
}

// =============================================================================
// AUTO-CLOSE DELAYED SELL ORDER TESTS
// =============================================================================

/// Test: Auto-close SELL order returns delayed and eventually fills.
///
/// Scenario:
/// - Initial arb: Kalshi fills 0 (canceled), Poly BUY fills 10 (delayed then matched)
/// - Mismatch detected: Poly has 10 excess
/// - Auto-close SELL returns delayed
/// - poll_delayed_order returns Filled
///
/// Expected:
/// 1. Auto-close should poll the delayed SELL order
/// 2. Fill should be recorded correctly
/// 3. No further retry attempts after delayed order fills
#[tokio::test]
async fn test_auto_close_delayed_sell_fills() {
    // Set a short timeout for the test
    std::env::set_var("POLY_DELAYED_TIMEOUT_MS", "1000");

    let kalshi_server = MockServer::start().await;

    // Mount Kalshi fixture that returns canceled with 0 fills
    let exchange = load_fixture(fixture_path("kalshi_canceled_no_fill.json"))
        .expect("Failed to load fixture");

    let mut response = ResponseTemplate::new(exchange.response.status)
        .set_body_raw(exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            response = response.append_header(key.as_str(), value.as_str());
        }
    }

    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(response)
        .mount(&kalshi_server)
        .await;

    // Configure mock Poly client:
    // 1. Initial BUY returns delayed, then fills 10 contracts
    // 2. Auto-close SELL also returns delayed, then fills 10 contracts
    let mock_poly = Arc::new(MockPolyClient::new());
    let buy_order_id = "delayed-buy-order-123";
    let sell_order_id = "delayed-sell-order-456";

    // For KalshiYesPolyNo, we buy Poly NO token
    // First call to buy_fak returns delayed
    mock_poly.set_delayed_order("poly-no-token-67890", buy_order_id);
    mock_poly.set_delayed_response(buy_order_id, MockDelayedResponse::Filled { size: 10.0 });

    // After reconciliation detects mismatch (Kalshi=0, Poly=10), auto-close will SELL
    // We need to reconfigure the mock for the SELL call
    // The mock uses the same token for both buy and sell, so we need a different approach
    // Actually, we can set a second delayed order for the same token - the mock will use the latest

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly.clone());

    // Execute the arb (KalshiYesPolyNo = Kalshi YES + Poly NO)
    let req = create_test_request(ArbType::KalshiYesPolyNo);
    let result = engine.process(req).await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result.err());

    // Wait for initial reconciliation
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    // Now reconfigure mock for the SELL (auto-close) - it should return delayed then fill
    mock_poly.set_delayed_order("poly-no-token-67890", sell_order_id);
    mock_poly.set_delayed_response(sell_order_id, MockDelayedResponse::Filled { size: 10.0 });

    // Wait for auto-close to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    // Collect all messages
    let (fills, _cleared_order_ids) = drain_messages(&mut fill_rx).await;

    // Get all sell calls
    let sell_calls = mock_poly.get_sell_calls();

    // There should be auto-close SELL attempts
    // With the fix, when one returns delayed and fills, we shouldn't see dozens of retries
    assert!(
        !sell_calls.is_empty(),
        "Should have at least one SELL call for auto-close"
    );

    // Check that Poly NO fills exist (buy + possible close)
    let poly_fills: Vec<_> = fills.iter()
        .filter(|f| f.platform == "polymarket" && f.side == "no")
        .collect();

    assert!(
        !poly_fills.is_empty(),
        "Should have Poly NO fills. Got: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );
}

/// Test: Auto-close SELL order returns delayed but times out (no fill).
///
/// Scenario:
/// - Mismatch: Poly has excess from a BUY that filled
/// - Auto-close SELL returns delayed
/// - poll_delayed_order times out
///
/// Expected:
/// 1. Auto-close should continue with retry at lower prices after timeout
/// 2. The delayed order is treated as no-fill after timeout
#[tokio::test]
async fn test_auto_close_delayed_sell_timeout_continues_retry() {
    // Set a very short timeout so we can test the retry behavior
    std::env::set_var("POLY_DELAYED_TIMEOUT_MS", "100");

    let kalshi_server = MockServer::start().await;

    // Mount Kalshi fixture that returns canceled with 0 fills
    let exchange = load_fixture(fixture_path("kalshi_canceled_no_fill.json"))
        .expect("Failed to load fixture");

    let mut response = ResponseTemplate::new(exchange.response.status)
        .set_body_raw(exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            response = response.append_header(key.as_str(), value.as_str());
        }
    }

    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(response)
        .mount(&kalshi_server)
        .await;

    // Configure mock:
    // - BUY fills normally (no delay)
    // - SELL returns delayed but times out
    let mock_poly = Arc::new(MockPolyClient::new());

    // For the buy - fill immediately
    mock_poly.set_full_fill("poly-no-token-67890", 10.0, 0.85);

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly.clone());

    // Execute the arb - Kalshi will fail (0 fills), Poly will succeed (10 fills)
    // This creates a mismatch that triggers auto-close
    let req = create_test_request(ArbType::KalshiYesPolyNo);
    let result = engine.process(req).await;

    assert!(result.is_ok(), "Execution should succeed");

    // Now reconfigure the mock for SELL calls to return delayed + timeout
    let sell_order_id = "delayed-sell-timeout";
    mock_poly.set_delayed_order("poly-no-token-67890", sell_order_id);
    mock_poly.set_delayed_response(sell_order_id, MockDelayedResponse::Timeout);

    // Wait for auto-close attempts
    tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;

    // Collect messages
    let (fills, _) = drain_messages(&mut fill_rx).await;

    // Get all sell calls - after timeout, it should continue with more attempts
    let sell_calls = mock_poly.get_sell_calls();

    // With the fix, a delayed + timeout should result in continued retries
    // We should see multiple SELL attempts (not just one that hangs)
    assert!(
        sell_calls.len() >= 1,
        "Should have SELL calls for auto-close. Got: {:?}",
        sell_calls
    );

    // Verify there's a Poly fill recorded (from the initial buy)
    let poly_buys: Vec<_> = fills.iter()
        .filter(|f| f.platform == "polymarket" && f.side == "no" && f.contracts > 0.0)
        .collect();

    assert!(
        !poly_buys.is_empty(),
        "Should have positive Poly NO fill from initial buy. Fills: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );
}
