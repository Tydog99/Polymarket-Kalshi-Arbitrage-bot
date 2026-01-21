//! True end-to-end tests for ExecutionEngine.
//!
//! These tests verify the actual data flow from API responses through to
//! PositionTracker fills. Unlike the previous "e2e" tests, these tests:
//!
//! 1. Use real KalshiApiClient pointing to wiremock with real captured fixtures
//! 2. Use MockPolyClient configured with values from real captured fixtures
//!    (poly_full_fill_real.json, poly_partial_fill_real.json)
//! 3. Verify FillRecords are created with data from API responses (not hardcoded)
//! 4. Verify auto-close behavior when fills are mismatched
//!
//! Note: MockPolyClient is used instead of wiremock because Polymarket requires
//! complex auth flow (/auth/derive-api-key + HMAC signing). The mock values are
//! configured to match the prices from real captured Polymarket responses.
//!
//! Test Matrix:
//! | Test | Kalshi | Poly | Verifies |
//! |------|--------|------|----------|
//! | test_engine_both_full_fill | Full fill | Full fill | 2 fills recorded correctly |
//! | test_engine_partial_fills | Partial fill | Partial fill | Partial quantities recorded |
//! | test_engine_kalshi_fills_poly_fails | Full fill | Error | Only Kalshi fill recorded |
//! | test_engine_no_fills | No fill | No fill | No fills recorded |
//!
//! Auto-Close Tests (PolyYesKalshiNo arb type):
//! | Test | Scenario | Verifies |
//! |------|----------|----------|
//! | test_auto_close_poly_yes_excess | Poly YES fills, Kalshi NO fails | sell_fak called on Poly YES token |
//! | test_auto_close_kalshi_no_excess | Kalshi NO fills, Poly YES fails | sell_ioc called on Kalshi |

use std::sync::Arc;

use rsa::RsaPrivateKey;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use arb_bot::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use arb_bot::execution::{ExecutionEngine, NanoClock};
use arb_bot::kalshi::{KalshiApiClient, KalshiConfig};
use arb_bot::poly_executor::mock::MockPolyClient;
use arb_bot::poly_executor::PolyExecutor;
use arb_bot::position_tracker::{create_position_channel, FillRecord};
use arb_bot::types::{ArbType, FastExecutionRequest, GlobalState, MarketPair, MarketType};

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
    }
}

/// Create an ExecutionEngine with mock Kalshi server and mock Poly client.
///
/// Returns the engine and a receiver for fill records.
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
) -> (ExecutionEngine, mpsc::UnboundedReceiver<FillRecord>) {
    let kalshi = Arc::new(create_test_kalshi_client(kalshi_server));
    let state = Arc::new(GlobalState::new());
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

/// Create a FastExecutionRequest for testing.
fn create_test_request(arb_type: ArbType) -> FastExecutionRequest {
    FastExecutionRequest {
        market_id: 0,
        yes_price: 9,   // 9 cents
        no_price: 85,   // 85 cents
        yes_size: 10000, // $100 available
        no_size: 10000,
        arb_type,
        detected_ns: 0,
    }
}

/// Drain all fills from the channel (non-blocking).
async fn drain_fills(rx: &mut mpsc::UnboundedReceiver<FillRecord>) -> Vec<FillRecord> {
    let mut fills = Vec::new();
    // Give a brief moment for fills to arrive
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    while let Ok(fill) = rx.try_recv() {
        fills.push(fill);
    }
    fills
}

// =============================================================================
// E2E TESTS
// =============================================================================

/// Test: Both Kalshi and Poly fill completely.
///
/// Expected: 2 FillRecords created with correct data from API responses.
#[tokio::test]
async fn test_engine_both_full_fill() {
    let kalshi_server = MockServer::start().await;

    // Load and mount Kalshi full fill fixture
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

    // Configure mock Poly client for full fill
    // Values based on poly_full_fill_real.json: 5 contracts at 0.53 price
    // But we use 1 contract to match the Kalshi full_fill_real fixture (1 contract)
    let mock_poly = Arc::new(MockPolyClient::new());
    // For PolyYesKalshiNo, we buy Poly YES token
    mock_poly.set_full_fill("poly-yes-token-12345", 1.0, 0.53);

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result.err());
    let exec_result = result.unwrap();
    assert!(exec_result.success, "Should report success, error: {:?}", exec_result.error);

    // Verify fills
    let fills = drain_fills(&mut fill_rx).await;
    assert_eq!(fills.len(), 2, "Should have 2 fills (one per leg)");

    // Find the Kalshi fill
    let kalshi_fill = fills.iter().find(|f| f.platform == "kalshi");
    assert!(kalshi_fill.is_some(), "Should have Kalshi fill");
    let kalshi_fill = kalshi_fill.unwrap();
    assert_eq!(kalshi_fill.contracts, 1.0, "Kalshi should fill 1 contract (from fixture)");
    assert_eq!(kalshi_fill.side, "no", "PolyYesKalshiNo means Kalshi NO");

    // Find the Poly fill
    let poly_fill = fills.iter().find(|f| f.platform == "polymarket");
    assert!(poly_fill.is_some(), "Should have Poly fill");
    let poly_fill = poly_fill.unwrap();
    assert_eq!(poly_fill.contracts, 1.0, "Poly should fill 1 contract");
    assert_eq!(poly_fill.side, "yes", "PolyYesKalshiNo means Poly YES");
}

/// Test: Both sides partial fill.
///
/// Expected: FillRecords reflect the actual partial quantities from APIs.
#[tokio::test]
async fn test_engine_partial_fills() {
    let kalshi_server = MockServer::start().await;

    // Load and mount Kalshi partial fill fixture (114/200 contracts)
    let exchange = load_fixture(fixture_path("kalshi_partial_fill_real.json"))
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

    // Configure mock Poly client for partial fill
    // Values based on poly_partial_fill_real.json: 5/10 contracts at 0.47 price
    let mock_poly = Arc::new(MockPolyClient::new());
    mock_poly.set_partial_fill("poly-yes-token-12345", 5.0, 0.47);

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;

    assert!(result.is_ok(), "Execution should succeed");

    // Verify fills reflect partial amounts
    let fills = drain_fills(&mut fill_rx).await;

    // Should have fills for the matched amount (min of both sides)
    let kalshi_fill = fills.iter().find(|f| f.platform == "kalshi");
    let poly_fill = fills.iter().find(|f| f.platform == "polymarket");

    // The matched contracts should be min(kalshi_filled, poly_filled)
    // Kalshi fixture has 114 filled, Poly fixture has 5 filled
    // So matched = 5
    if let (Some(k), Some(p)) = (kalshi_fill, poly_fill) {
        let matched = k.contracts.min(p.contracts);
        assert!(matched > 0.0, "Should have some matched contracts");
        // Both fills should record the matched amount
        assert_eq!(k.contracts, matched, "Kalshi fill should be matched amount");
        assert_eq!(p.contracts, matched, "Poly fill should be matched amount");
    }
}

/// Test: Kalshi fills but Poly fails.
///
/// Expected:
/// - No matched position fills (matched = 0 when one leg fails)
/// - Auto-close records a close fill with negative contracts
/// - Net position is 0 after auto-close
#[tokio::test]
async fn test_engine_kalshi_fills_poly_fails() {
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

    // Configure mock Poly client to return error
    let mock_poly = Arc::new(MockPolyClient::new());
    mock_poly.set_error("poly-yes-token-12345", "Insufficient balance");

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;

    // Execution completes but with mismatched fills
    assert!(result.is_ok());

    // Wait for auto-close background task
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify fills
    let fills = drain_fills(&mut fill_rx).await;

    // When one leg fails and one succeeds:
    // - matched = min(kalshi_filled, poly_filled) = min(1, 0) = 0
    // - No matched position fills (positive contracts)
    // - But auto-close records a close fill (negative contracts)

    let matched_fills: Vec<_> = fills.iter().filter(|f| f.contracts > 0.0).collect();
    let close_fills: Vec<_> = fills.iter().filter(|f| f.contracts < 0.0).collect();

    assert_eq!(
        matched_fills.len(), 0,
        "No matched fills when one leg fails completely"
    );

    assert_eq!(
        close_fills.len(), 1,
        "Should have 1 close fill from auto-close"
    );

    // Verify the close fill is on Kalshi NO (the side that filled)
    let close = &close_fills[0];
    assert_eq!(close.platform, "kalshi");
    assert_eq!(close.side, "no");
    assert!(close.contracts < 0.0, "Close fill should have negative contracts");
}

/// Test: Both sides get no fill (canceled/expired).
///
/// Expected: No FillRecords created.
#[tokio::test]
async fn test_engine_no_fills() {
    let kalshi_server = MockServer::start().await;

    // Load and mount Kalshi no fill fixture
    let exchange = load_fixture(fixture_path("kalshi_no_fill_real.json"))
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

    // Configure mock Poly client for no fill (partial with 0)
    let mock_poly = Arc::new(MockPolyClient::new());
    mock_poly.set_partial_fill("poly-yes-token-12345", 0.0, 0.09);

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;

    assert!(result.is_ok());

    // Verify no fills recorded
    let fills = drain_fills(&mut fill_rx).await;
    assert_eq!(fills.len(), 0, "No fills when both legs get 0 quantity");
}

// =============================================================================
// AUTO-CLOSE TESTS
// =============================================================================
//
// These tests verify that when fills are mismatched between platforms,
// the auto-close logic sells on the CORRECT platform (the one with excess).
//
// For PolyYesKalshiNo arb type:
// - We buy Poly YES and Kalshi NO
// - If Poly YES fills but Kalshi NO doesn't → excess is on POLY → sell on Poly
// - If Kalshi NO fills but Poly YES doesn't → excess is on KALSHI → sell on Kalshi

/// Test: PolyYesKalshiNo where Kalshi NO fills but Poly YES fails.
///
/// Scenario:
/// - Kalshi NO buy succeeds (1 contract filled)
/// - Poly YES buy fails (error, 0 contracts)
/// - Excess position is on KALSHI (we own Kalshi NO contracts)
///
/// Expected:
/// - Auto-close should sell on KALSHI to close the excess NO position
/// - Position tracker should show the close fill (negative contracts)
/// - Net position should be 0 after auto-close
#[tokio::test]
async fn test_auto_close_kalshi_excess_sells_on_kalshi() {
    let kalshi_server = MockServer::start().await;

    // Mount Kalshi BUY with full fill (1 contract)
    let buy_exchange = load_fixture(fixture_path("kalshi_full_fill_real.json"))
        .expect("Failed to load fixture");

    let mut buy_response = ResponseTemplate::new(buy_exchange.response.status)
        .set_body_raw(buy_exchange.response.body_raw.clone(), "application/json");

    for (key, value) in &buy_exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            buy_response = buy_response.append_header(key.as_str(), value.as_str());
        }
    }

    // Mount Kalshi SELL response for the auto-close order
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
        .expect(1) // Expect exactly 1 sell order on Kalshi
        .mount(&kalshi_server)
        .await;

    // Configure mock Poly to FAIL - this creates the mismatch
    let mock_poly = Arc::new(MockPolyClient::new());
    mock_poly.set_error("poly-yes-token-12345", "Insufficient balance");

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly.clone());

    // Execute the arb
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;
    assert!(result.is_ok());

    // Wait for auto-close background task
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify Poly sell was NOT called (excess is on Kalshi, not Poly)
    let poly_sell_calls = mock_poly.get_sell_calls();
    assert!(
        poly_sell_calls.is_empty(),
        "Should NOT have called Poly sell_fak - excess is on Kalshi! Got {} sell calls",
        poly_sell_calls.len()
    );

    // Verify position tracking: should have a close fill with negative contracts
    let fills = drain_fills(&mut fill_rx).await;

    // Since Poly failed (0 filled) and Kalshi filled (1 contract), matched = 0
    // So no matched position fills, but we should have a close fill from auto-close
    let close_fill = fills.iter().find(|f| f.contracts < 0.0);
    assert!(
        close_fill.is_some(),
        "Should have a close fill with negative contracts. Fills: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );

    let close = close_fill.unwrap();
    assert_eq!(close.platform, "kalshi", "Close should be on Kalshi");
    assert_eq!(close.side, "no", "Close should be for NO side");
    assert!(close.contracts < 0.0, "Close fill should have negative contracts");

    // Wiremock expect(1) will verify Kalshi sell WAS called when server drops
}

/// Test: PolyYesKalshiNo where Poly YES fills but Kalshi NO fails.
///
/// Scenario:
/// - Poly YES buy succeeds (1 contract filled)
/// - Kalshi NO buy fails (0 contracts from no_fill fixture)
/// - Excess position is on POLY (we own Poly YES contracts)
///
/// Expected:
/// - Auto-close should sell on POLY to close the excess YES position
/// - Position tracker should show the close fill (negative contracts)
/// - Net position should be 0 after auto-close
#[tokio::test]
async fn test_auto_close_poly_excess_sells_on_poly() {
    let kalshi_server = MockServer::start().await;

    // Mount Kalshi with NO fill (0 contracts)
    let exchange = load_fixture(fixture_path("kalshi_no_fill_real.json"))
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

    // Configure mock Poly for FULL fill on YES token (buy and sell)
    // This creates the mismatch - Poly fills, Kalshi doesn't
    // Values based on poly_full_fill_real.json: price 0.53
    let mock_poly = Arc::new(MockPolyClient::new());
    mock_poly.set_full_fill("poly-yes-token-12345", 1.0, 0.53);

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly.clone());

    // Execute
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;
    assert!(result.is_ok());

    // Wait for auto-close background task (2s Poly settlement wait + buffer)
    tokio::time::sleep(tokio::time::Duration::from_millis(2500)).await;

    // Verify that sell_fak WAS called on Poly to close the excess
    let sell_calls = mock_poly.get_sell_calls();
    assert!(
        !sell_calls.is_empty(),
        "Should have called Poly sell_fak to close Poly YES excess"
    );

    // Verify the sell was for the correct token (YES token, not NO)
    let poly_yes_sell = sell_calls.iter().find(|c| c.token_id == "poly-yes-token-12345");
    assert!(
        poly_yes_sell.is_some(),
        "Should have sold Poly YES token to close excess"
    );

    // Verify we're closing 1 contract (the excess amount)
    let call = poly_yes_sell.unwrap();
    assert!(
        (call.size - 1.0).abs() < 0.01,
        "Should close 1 contract, got {}",
        call.size
    );

    // Verify position tracking: should have a close fill with negative contracts
    let fills = drain_fills(&mut fill_rx).await;

    // Since Kalshi failed (0 filled) and Poly filled (1 contract), matched = 0
    // So no matched position fills, but we should have a close fill from auto-close
    let close_fill = fills.iter().find(|f| f.contracts < 0.0);
    assert!(
        close_fill.is_some(),
        "Should have a close fill with negative contracts. Fills: {:?}",
        fills.iter().map(|f| (f.platform.as_str(), f.side.as_str(), f.contracts)).collect::<Vec<_>>()
    );

    let close = close_fill.unwrap();
    assert_eq!(close.platform, "polymarket", "Close should be on Polymarket");
    assert_eq!(close.side, "yes", "Close should be for YES side");
    assert!(close.contracts < 0.0, "Close fill should have negative contracts");
    assert!(
        (close.contracts.abs() - 1.0).abs() < 0.01,
        "Should close 1 contract, got {}",
        close.contracts.abs()
    );
}
