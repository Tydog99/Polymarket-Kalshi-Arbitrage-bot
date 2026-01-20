//! Integration tests for execute_leg() using wiremock mock servers and real captured fixtures.
//!
//! These tests verify that `execute_leg()` correctly:
//! 1. Makes HTTP requests to Kalshi API
//! 2. Parses responses correctly
//! 3. Returns appropriate LegResult (success/failure)

use std::sync::Arc;

use rsa::RsaPrivateKey;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::replay_harness::{load_fixture, setup_mock_server};
use trading::execution::{execute_leg, ExecutionClients, LegRequest, OrderAction, OutcomeSide, Platform};
use trading::kalshi::{KalshiApiClient, KalshiConfig};

// ============================================================================
// TEST HELPERS
// ============================================================================

fn fixture_path(name: &str) -> String {
    format!(
        "{}/tests/integration/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    )
}

/// Mount a Kalshi order fixture for execute_leg tests.
///
/// The KalshiApiClient makes requests to `/portfolio/orders`, but the captured
/// fixtures have the full path `/trade-api/v2/portfolio/orders`. This helper
/// mounts the fixture with a regex that matches just `/portfolio/orders`.
async fn mount_kalshi_order_fixture(server: &MockServer, fixture_name: &str) {
    let exchange = load_fixture(fixture_path(fixture_name)).expect("Failed to load fixture");

    // Build response template with status and body
    let mut response = ResponseTemplate::new(exchange.response.status)
        .set_body_raw(exchange.response.body_raw.clone(), "application/json");

    // Add response headers (except content-length which wiremock handles)
    for (key, value) in &exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            response = response.append_header(key.as_str(), value.as_str());
        }
    }

    // Match POST requests to /portfolio/orders endpoint
    Mock::given(method("POST"))
        .and(path_regex(".*portfolio/orders.*"))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Create a test KalshiConfig with a generated RSA key.
///
/// The key is generated at runtime since we only need it for signing
/// (which the mock server doesn't verify).
fn create_test_kalshi_config() -> KalshiConfig {
    let mut rng = rand::thread_rng();
    // Use 2048 bits - the RSA PSS signing algorithm requires a minimum key size
    let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("Failed to generate RSA key");

    KalshiConfig {
        api_key_id: "test-api-key".to_string(),
        private_key,
    }
}

/// Create a KalshiApiClient pointing at a mock server.
fn create_test_kalshi_client(server: &MockServer) -> KalshiApiClient {
    let config = create_test_kalshi_config();
    // The mock server URI doesn't include the /trade-api/v2 prefix,
    // but the client adds paths like /portfolio/orders on top of the base URL.
    // We need to match how the replay harness mounts fixtures.
    KalshiApiClient::new_with_base_url(config, &server.uri())
}

/// Create ExecutionClients with only Kalshi client configured.
fn create_test_execution_clients(kalshi_client: KalshiApiClient) -> ExecutionClients {
    ExecutionClients {
        kalshi: Some(Arc::new(kalshi_client)),
        polymarket: None,
    }
}

// ============================================================================
// TEST: KALSHI FULL FILL
// ============================================================================

/// Test execute_leg with a real Kalshi full fill response.
///
/// Scenario:
/// - Ticker: KXNBA-26-SAS
/// - Requested: 1 contract at 9c YES
/// - Result: Full fill (1/1 contracts executed)
/// - Expected: LegResult::success = true
#[tokio::test]
async fn test_execute_leg_kalshi_full_fill() {
    let server = setup_mock_server().await;

    // Mount the real captured fixture
    mount_kalshi_order_fixture(&server, "kalshi_full_fill_real.json").await;

    // Create test client and execution clients
    let kalshi_client = create_test_kalshi_client(&server);
    let clients = create_test_execution_clients(kalshi_client);

    // Create the leg request matching the fixture
    let req = LegRequest {
        leg_id: "test-leg-full-fill",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::Yes,
        price_cents: 9,
        contracts: 1,
        kalshi_ticker: Some("KXNBA-26-SAS"),
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify success
    assert!(
        result.success,
        "Full fill should succeed, but got error: {:?}",
        result.error
    );
    assert!(result.error.is_none(), "Full fill should have no error");
    assert!(result.latency_ns > 0, "Should record latency");
}

// ============================================================================
// TEST: KALSHI PARTIAL FILL
// ============================================================================

/// Test execute_leg with a real Kalshi partial fill response.
///
/// Scenario:
/// - Ticker: KXNEWPOPE-70-PPAR
/// - Requested: 200 contracts at 9c YES
/// - Result: Partial fill (114/200 contracts executed)
/// - Expected: LegResult::success = true (partial fills are still successful)
#[tokio::test]
async fn test_execute_leg_kalshi_partial_fill() {
    let server = setup_mock_server().await;

    // Mount the real captured fixture
    mount_kalshi_order_fixture(&server, "kalshi_partial_fill_real.json").await;

    // Create test client and execution clients
    let kalshi_client = create_test_kalshi_client(&server);
    let clients = create_test_execution_clients(kalshi_client);

    // Create the leg request matching the fixture
    let req = LegRequest {
        leg_id: "test-leg-partial-fill",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::Yes,
        price_cents: 9,
        contracts: 200,
        kalshi_ticker: Some("KXNEWPOPE-70-PPAR"),
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify success - partial fills are considered successful
    // The order was placed and some contracts were filled
    assert!(result.success, "Partial fill should succeed");
    assert!(result.error.is_none(), "Partial fill should have no error");
    assert!(result.latency_ns > 0, "Should record latency");
}

// ============================================================================
// TEST: KALSHI NO FILL
// ============================================================================

/// Test execute_leg with a real Kalshi no fill response.
///
/// Scenario:
/// - Ticker: KXNBA-26-SAS
/// - Requested: 1 contract at 5c YES (below market)
/// - Result: No fill (0/1 contracts executed)
/// - Expected: LegResult::success = true (no fill is not an error)
#[tokio::test]
async fn test_execute_leg_kalshi_no_fill() {
    let server = setup_mock_server().await;

    // Mount the real captured fixture
    mount_kalshi_order_fixture(&server, "kalshi_no_fill.json").await;

    // Create test client and execution clients
    let kalshi_client = create_test_kalshi_client(&server);
    let clients = create_test_execution_clients(kalshi_client);

    // Create the leg request matching the fixture
    let req = LegRequest {
        leg_id: "test-leg-no-fill",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::Yes,
        price_cents: 5,  // Below market price
        contracts: 1,
        kalshi_ticker: Some("KXNBA-26-SAS"),
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify success - no fill is not an error, the order was successfully
    // placed but nothing was available at the price
    assert!(result.success, "No fill should succeed (order placed successfully)");
    assert!(result.error.is_none(), "No fill should have no error");
    assert!(result.latency_ns > 0, "Should record latency");
}

// ============================================================================
// TEST: KALSHI MARKET NOT FOUND
// ============================================================================

/// Test execute_leg with a real Kalshi market not found error.
///
/// Scenario:
/// - Ticker: KXNFLAFCCHAMP-25 (invalid/expired market)
/// - Result: 404 Not Found
/// - Expected: LegResult::success = false with error message
#[tokio::test]
async fn test_execute_leg_kalshi_market_not_found() {
    let server = setup_mock_server().await;

    // Mount the real captured fixture
    mount_kalshi_order_fixture(&server, "kalshi_market_not_found.json").await;

    // Create test client and execution clients
    let kalshi_client = create_test_kalshi_client(&server);
    let clients = create_test_execution_clients(kalshi_client);

    // Create the leg request matching the fixture
    let req = LegRequest {
        leg_id: "test-leg-market-not-found",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::Yes,
        price_cents: 71,
        contracts: 1,
        kalshi_ticker: Some("KXNFLAFCCHAMP-25"),
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify failure
    assert!(!result.success, "Market not found should fail");
    assert!(result.error.is_some(), "Should have error message");

    let error_msg = result.error.unwrap();
    assert!(
        error_msg.contains("404") || error_msg.contains("market_not_found") || error_msg.contains("error"),
        "Error should indicate market not found: {}",
        error_msg
    );
    assert!(result.latency_ns > 0, "Should record latency even on failure");
}

// ============================================================================
// TEST: MISSING KALSHI TICKER
// ============================================================================

/// Test execute_leg with missing kalshi_ticker (should fail early).
#[tokio::test]
async fn test_execute_leg_kalshi_missing_ticker() {
    let server = setup_mock_server().await;

    // We don't need to mount any fixture since the error happens before HTTP request
    let kalshi_client = create_test_kalshi_client(&server);
    let clients = create_test_execution_clients(kalshi_client);

    // Create a leg request with missing ticker
    let req = LegRequest {
        leg_id: "test-leg-missing-ticker",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::Yes,
        price_cents: 50,
        contracts: 10,
        kalshi_ticker: None,  // Missing!
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify failure due to missing ticker
    assert!(!result.success, "Missing ticker should fail");
    assert!(result.error.is_some(), "Should have error message");
    assert!(
        result.error.unwrap().contains("Missing kalshi_ticker"),
        "Error should mention missing ticker"
    );
}

// ============================================================================
// TEST: MISSING KALSHI CLIENT
// ============================================================================

/// Test execute_leg with missing Kalshi client (should fail early).
#[tokio::test]
async fn test_execute_leg_kalshi_missing_client() {
    // Create execution clients with no Kalshi client
    let clients = ExecutionClients {
        kalshi: None,
        polymarket: None,
    };

    // Create a valid leg request
    let req = LegRequest {
        leg_id: "test-leg-missing-client",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::Yes,
        price_cents: 50,
        contracts: 10,
        kalshi_ticker: Some("TEST-TICKER"),
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify failure due to missing client
    assert!(!result.success, "Missing client should fail");
    assert!(result.error.is_some(), "Should have error message");
    assert!(
        result.error.unwrap().contains("not available"),
        "Error should mention client not available"
    );
}

// ============================================================================
// TEST: DRY RUN MODE
// ============================================================================

/// Test execute_leg in dry run mode (should succeed without making HTTP request).
#[tokio::test]
async fn test_execute_leg_kalshi_dry_run() {
    // We can use empty execution clients in dry run mode
    let clients = ExecutionClients {
        kalshi: None,
        polymarket: None,
    };

    // Create a valid leg request
    let req = LegRequest {
        leg_id: "test-leg-dry-run",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::Yes,
        price_cents: 50,
        contracts: 10,
        kalshi_ticker: Some("TEST-TICKER"),
        poly_token: None,
    };

    // Execute in dry run mode
    let result = execute_leg(&req, &clients, true).await;

    // Verify success (dry run always succeeds if ticker is present)
    assert!(result.success, "Dry run should succeed");
    assert!(result.error.is_none(), "Dry run should have no error");
}

// ============================================================================
// TEST: SELL ORDER
// ============================================================================

/// Test execute_leg with a sell action (uses same fixture format as buy).
#[tokio::test]
async fn test_execute_leg_kalshi_sell() {
    let server = setup_mock_server().await;

    // Mount the full fill fixture (response format is same for buy/sell)
    mount_kalshi_order_fixture(&server, "kalshi_full_fill_real.json").await;

    // Create test client and execution clients
    let kalshi_client = create_test_kalshi_client(&server);
    let clients = create_test_execution_clients(kalshi_client);

    // Create a SELL leg request
    let req = LegRequest {
        leg_id: "test-leg-sell",
        platform: Platform::Kalshi,
        action: OrderAction::Sell,
        side: OutcomeSide::Yes,
        price_cents: 9,
        contracts: 1,
        kalshi_ticker: Some("KXNBA-26-SAS"),
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify success
    assert!(result.success, "Sell order should succeed");
    assert!(result.error.is_none(), "Sell should have no error");
}

// ============================================================================
// TEST: NO SIDE ORDER
// ============================================================================

/// Test execute_leg with NO side (as opposed to YES).
#[tokio::test]
async fn test_execute_leg_kalshi_no_side() {
    let server = setup_mock_server().await;

    // Mount the full fill fixture
    mount_kalshi_order_fixture(&server, "kalshi_full_fill_real.json").await;

    // Create test client and execution clients
    let kalshi_client = create_test_kalshi_client(&server);
    let clients = create_test_execution_clients(kalshi_client);

    // Create a request for NO side
    let req = LegRequest {
        leg_id: "test-leg-no-side",
        platform: Platform::Kalshi,
        action: OrderAction::Buy,
        side: OutcomeSide::No,
        price_cents: 91,  // NO price = 100 - YES price (9)
        contracts: 1,
        kalshi_ticker: Some("KXNBA-26-SAS"),
        poly_token: None,
    };

    // Execute the leg (not a dry run)
    let result = execute_leg(&req, &clients, false).await;

    // Verify success
    assert!(result.success, "NO side order should succeed");
    assert!(result.error.is_none(), "Should have no error");
}
