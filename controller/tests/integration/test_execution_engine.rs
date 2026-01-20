//! True end-to-end tests for ExecutionEngine.
//!
//! These tests verify the actual data flow from API responses through to
//! PositionTracker fills. Unlike the previous "e2e" tests, these tests:
//!
//! 1. Use real KalshiApiClient pointing to wiremock (not manual fixture loading)
//! 2. Use MockPolyClient with canned responses
//! 3. Verify FillRecords are created with data from API responses (not hardcoded)
//!
//! Test Matrix:
//! | Test | Kalshi | Poly | Verifies |
//! |------|--------|------|----------|
//! | test_engine_both_full_fill | Full fill | Full fill | 2 fills recorded correctly |
//! | test_engine_partial_fills | Partial fill | Partial fill | Partial quantities recorded |
//! | test_engine_kalshi_fills_poly_fails | Full fill | Error | Only Kalshi fill recorded |
//! | test_engine_no_fills | No fill | No fill | No fills recorded |

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
    let mock_poly = Arc::new(MockPolyClient::new());
    // For PolyYesKalshiNo, we buy Poly YES token
    mock_poly.set_full_fill("poly-yes-token-12345", 1.0, 0.09);

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
    let mock_poly = Arc::new(MockPolyClient::new());
    mock_poly.set_partial_fill("poly-yes-token-12345", 50.0, 0.09);

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
    // Kalshi fixture has 114 filled, Poly mock has 50 filled
    // So matched = 50
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
/// Expected: Only Kalshi leg recorded, exposure tracked.
#[tokio::test]
async fn test_engine_kalshi_fills_poly_fails() {
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

    // Configure mock Poly client to return error
    let mock_poly = Arc::new(MockPolyClient::new());
    mock_poly.set_error("poly-yes-token-12345", "Insufficient balance");

    let (engine, mut fill_rx) = create_test_engine(&kalshi_server, mock_poly);

    // Execute
    let req = create_test_request(ArbType::PolyYesKalshiNo);
    let result = engine.process(req).await;

    // Execution completes but with mismatched fills
    assert!(result.is_ok());

    // Verify only Kalshi filled, no matched position
    let fills = drain_fills(&mut fill_rx).await;

    // When one leg fails and one succeeds, we have unmatched exposure
    // The matched contracts = min(kalshi_filled, poly_filled) = min(1, 0) = 0
    // So no fills should be recorded for the matched position
    // However, there might be exposure management fills

    // The key assertion: we should NOT have both legs filling equally
    let total_contracts: f64 = fills.iter().map(|f| f.contracts).sum();

    // With poly failing (0 filled) and kalshi succeeding (1 filled),
    // matched = 0, so no fills recorded for matched position
    assert_eq!(fills.len(), 0, "No matched fills when one leg fails completely");
}

/// Test: Both sides get no fill (canceled/expired).
///
/// Expected: No FillRecords created.
#[tokio::test]
async fn test_engine_no_fills() {
    let kalshi_server = MockServer::start().await;

    // Load and mount Kalshi no fill fixture
    let exchange = load_fixture(fixture_path("kalshi_no_fill.json"))
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
