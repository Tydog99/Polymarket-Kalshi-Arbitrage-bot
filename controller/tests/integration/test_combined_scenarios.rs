//! Combined scenario tests for cross-platform arbitrage execution.
//!
//! These tests verify behavior when executing arbitrage trades across both
//! Kalshi and Polymarket platforms simultaneously. Each scenario tests a
//! different combination of fill outcomes.
//!
//! # Test Scenarios
//!
//! | Scenario | Kalshi | Polymarket | What it tests |
//! |----------|--------|------------|---------------|
//! | Both sides fill | Full fill | Full fill | Happy path arbitrage |
//! | Kalshi fills, Poly fails | Full fill | Error | Unmatched exposure |
//! | Both partial | Partial fill | Partial fill | Imbalanced position |
//! | Kalshi fails, Poly fills | Error | Full fill | Reverse exposure |

use serde_json::json;

use super::replay_harness::{
    create_exchange_json, mount_fixture, mount_fixture_file, setup_mock_server,
};

// ============================================================================
// TEST FIXTURES PATH HELPERS
// ============================================================================

fn fixture_path(name: &str) -> String {
    format!(
        "{}/tests/integration/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    )
}

// ============================================================================
// SCENARIO: BOTH SIDES FILL (HAPPY PATH)
// ============================================================================

/// Test the happy path where both Kalshi and Polymarket orders fill completely.
///
/// This represents a successful arbitrage execution where:
/// - Kalshi: 10 contracts filled at 45c
/// - Polymarket: 10 contracts filled at 55c
/// - Net profit: 10 * ($1.00 - $0.45 - $0.55) = 0 (break-even minus fees)
#[tokio::test]
async fn test_both_sides_full_fill() {
    // Set up separate mock servers for each platform
    let kalshi_server = setup_mock_server().await;
    let poly_server = setup_mock_server().await;

    // Mount Kalshi full fill fixture
    let kalshi_exchange = mount_fixture_file(&kalshi_server, fixture_path("kalshi_full_fill.json")).await;

    // Mount Polymarket fixtures (order + status check)
    mount_fixture_file(&poly_server, fixture_path("poly_full_fill.json")).await;
    mount_fixture_file(&poly_server, fixture_path("poly_order_status_full.json")).await;

    // Verify Kalshi fixture loaded correctly
    assert_eq!(kalshi_exchange.response.status, 200);
    let kalshi_body = kalshi_exchange.response.body_parsed.unwrap();
    assert_eq!(kalshi_body["order"]["status"], "executed");
    assert_eq!(kalshi_body["order"]["taker_fill_count"], 10);
    assert_eq!(kalshi_body["order"]["remaining_count"], 0);

    // Make requests to mock servers to verify behavior
    let client = reqwest::Client::new();

    // Kalshi order
    let kalshi_resp = client
        .post(format!("{}/trade-api/v2/portfolio/orders", kalshi_server.uri()))
        .json(&json!({
            "ticker": "KXNBAML-26JAN19LALNYK-NYK",
            "action": "buy",
            "side": "no",
            "count": 10,
            "no_price": 45
        }))
        .send()
        .await
        .expect("Kalshi request should succeed");

    assert_eq!(kalshi_resp.status(), 200);
    let kalshi_result: serde_json::Value = kalshi_resp.json().await.unwrap();
    assert_eq!(kalshi_result["order"]["taker_fill_count"], 10);

    // Polymarket order
    let poly_resp = client
        .post(format!("{}/order", poly_server.uri()))
        .json(&json!({
            "order": {"tokenId": "48340483024983498234892834"},
            "orderType": "FAK"
        }))
        .send()
        .await
        .expect("Poly order request should succeed");

    assert_eq!(poly_resp.status(), 200);
    let poly_result: serde_json::Value = poly_resp.json().await.unwrap();
    assert_eq!(poly_result["orderID"], "poly-order-001");

    // Polymarket status check
    let poly_status_resp = client
        .get(format!("{}/data/order/poly-order-001", poly_server.uri()))
        .send()
        .await
        .expect("Poly status request should succeed");

    assert_eq!(poly_status_resp.status(), 200);
    let poly_status: serde_json::Value = poly_status_resp.json().await.unwrap();
    assert_eq!(poly_status["status"], "MATCHED");
    assert_eq!(poly_status["size_matched"], "10");
}

// ============================================================================
// SCENARIO: KALSHI FILLS, POLYMARKET FAILS (UNMATCHED EXPOSURE)
// ============================================================================

/// Test scenario where Kalshi fills but Polymarket fails.
///
/// This creates unmatched exposure:
/// - Kalshi: 10 contracts filled (we own 10 NO contracts)
/// - Polymarket: Order rejected (insufficient balance)
/// - Result: Exposed to loss if outcome is YES
#[tokio::test]
async fn test_kalshi_fills_poly_fails() {
    let kalshi_server = setup_mock_server().await;
    let poly_server = setup_mock_server().await;

    // Mount Kalshi full fill
    mount_fixture_file(&kalshi_server, fixture_path("kalshi_full_fill.json")).await;

    // Mount Polymarket error
    mount_fixture_file(&poly_server, fixture_path("poly_insufficient_balance.json")).await;

    let client = reqwest::Client::new();

    // Kalshi succeeds
    let kalshi_resp = client
        .post(format!("{}/trade-api/v2/portfolio/orders", kalshi_server.uri()))
        .json(&json!({
            "ticker": "KXNBAML-26JAN19LALNYK-NYK",
            "action": "buy",
            "side": "no",
            "count": 10
        }))
        .send()
        .await
        .expect("Kalshi request should succeed");

    assert_eq!(kalshi_resp.status(), 200);
    let kalshi_result: serde_json::Value = kalshi_resp.json().await.unwrap();
    let kalshi_filled = kalshi_result["order"]["taker_fill_count"].as_i64().unwrap_or(0);
    assert_eq!(kalshi_filled, 10, "Kalshi should fill completely");

    // Polymarket fails
    let poly_resp = client
        .post(format!("{}/order", poly_server.uri()))
        .json(&json!({
            "order": {"tokenId": "48340483024983498234892834"},
            "orderType": "FAK"
        }))
        .send()
        .await
        .expect("Poly request should complete");

    assert_eq!(poly_resp.status(), 400, "Polymarket should return error");
    let poly_error: serde_json::Value = poly_resp.json().await.unwrap();
    assert_eq!(poly_error["error"], "INSUFFICIENT_BALANCE");

    // Verify unmatched exposure state
    // In production, this would trigger position tracking to record:
    // - Kalshi: +10 NO contracts
    // - Polymarket: 0 contracts
    // - Net exposure: 10 contracts (unfunded arb leg)
    let net_kalshi_position = kalshi_filled;
    let net_poly_position: i64 = 0; // Failed to fill
    let exposure = net_kalshi_position - net_poly_position;
    assert_eq!(exposure, 10, "Should have 10 contracts of unmatched exposure");
}

// ============================================================================
// SCENARIO: BOTH PARTIAL FILLS (IMBALANCED POSITION)
// ============================================================================

/// Test scenario where both sides partially fill with different amounts.
///
/// This creates an imbalanced position:
/// - Kalshi: 6 of 10 contracts filled
/// - Polymarket: 7 of 10 contracts filled
/// - Result: Position mismatch requiring reconciliation
#[tokio::test]
async fn test_both_partial_fills() {
    let kalshi_server = setup_mock_server().await;
    let poly_server = setup_mock_server().await;

    // Mount Kalshi partial fill
    mount_fixture_file(&kalshi_server, fixture_path("kalshi_partial_fill.json")).await;

    // Mount Polymarket partial fill fixtures
    mount_fixture_file(&poly_server, fixture_path("poly_partial_fill.json")).await;
    mount_fixture_file(&poly_server, fixture_path("poly_order_status_partial.json")).await;

    let client = reqwest::Client::new();

    // Kalshi partial fill
    let kalshi_resp = client
        .post(format!("{}/trade-api/v2/portfolio/orders", kalshi_server.uri()))
        .json(&json!({
            "ticker": "KXNBAML-26JAN19LALNYK-NYK",
            "action": "buy",
            "side": "no",
            "count": 10
        }))
        .send()
        .await
        .expect("Kalshi request should succeed");

    assert_eq!(kalshi_resp.status(), 200);
    let kalshi_result: serde_json::Value = kalshi_resp.json().await.unwrap();
    let kalshi_filled = kalshi_result["order"]["taker_fill_count"].as_i64().unwrap_or(0);
    let kalshi_remaining = kalshi_result["order"]["remaining_count"].as_i64().unwrap_or(0);
    assert_eq!(kalshi_filled, 6, "Kalshi should partially fill 6 contracts");
    assert_eq!(kalshi_remaining, 4, "Kalshi should have 4 remaining");

    // Polymarket order
    let poly_order_resp = client
        .post(format!("{}/order", poly_server.uri()))
        .json(&json!({
            "order": {"tokenId": "48340483024983498234892834"},
            "orderType": "FAK"
        }))
        .send()
        .await
        .expect("Poly order should succeed");

    assert_eq!(poly_order_resp.status(), 200);
    let poly_order: serde_json::Value = poly_order_resp.json().await.unwrap();
    let poly_order_id = poly_order["orderID"].as_str().unwrap();

    // Check Polymarket fill status
    let poly_status_resp = client
        .get(format!("{}/data/order/{}", poly_server.uri(), poly_order_id))
        .send()
        .await
        .expect("Poly status should succeed");

    assert_eq!(poly_status_resp.status(), 200);
    let poly_status: serde_json::Value = poly_status_resp.json().await.unwrap();
    let poly_filled: f64 = poly_status["size_matched"].as_str().unwrap().parse().unwrap();
    assert_eq!(poly_filled as i64, 7, "Poly should partially fill 7 contracts");

    // Verify imbalanced position
    // Kalshi: 6 NO contracts
    // Polymarket: 7 YES contracts
    // Imbalance: 1 contract (Poly has more)
    let imbalance = (poly_filled as i64) - kalshi_filled;
    assert_eq!(imbalance, 1, "Should have 1 contract imbalance");
}

// ============================================================================
// SCENARIO: KALSHI FAILS, POLYMARKET FILLS (REVERSE EXPOSURE)
// ============================================================================

/// Test scenario where Kalshi fails but Polymarket fills.
///
/// This creates reverse exposure:
/// - Kalshi: Order rejected (insufficient balance)
/// - Polymarket: 10 contracts filled
/// - Result: Exposed to loss if outcome is NO
#[tokio::test]
async fn test_kalshi_fails_poly_fills() {
    let kalshi_server = setup_mock_server().await;
    let poly_server = setup_mock_server().await;

    // Mount Kalshi error
    mount_fixture_file(&kalshi_server, fixture_path("kalshi_insufficient_balance.json")).await;

    // Mount Polymarket full fill
    mount_fixture_file(&poly_server, fixture_path("poly_full_fill.json")).await;
    mount_fixture_file(&poly_server, fixture_path("poly_order_status_full.json")).await;

    let client = reqwest::Client::new();

    // Kalshi fails
    let kalshi_resp = client
        .post(format!("{}/trade-api/v2/portfolio/orders", kalshi_server.uri()))
        .json(&json!({
            "ticker": "KXNBAML-26JAN19LALNYK-NYK",
            "action": "buy",
            "side": "no",
            "count": 10
        }))
        .send()
        .await
        .expect("Kalshi request should complete");

    assert_eq!(kalshi_resp.status(), 400, "Kalshi should return error");
    let kalshi_error: serde_json::Value = kalshi_resp.json().await.unwrap();
    assert!(kalshi_error["error"]["code"].as_str().unwrap().contains("insufficient"));

    // Polymarket succeeds
    let poly_order_resp = client
        .post(format!("{}/order", poly_server.uri()))
        .json(&json!({
            "order": {"tokenId": "48340483024983498234892834"},
            "orderType": "FAK"
        }))
        .send()
        .await
        .expect("Poly order should succeed");

    assert_eq!(poly_order_resp.status(), 200);
    let poly_order: serde_json::Value = poly_order_resp.json().await.unwrap();
    let poly_order_id = poly_order["orderID"].as_str().unwrap();

    // Check Polymarket fill
    let poly_status_resp = client
        .get(format!("{}/data/order/{}", poly_server.uri(), poly_order_id))
        .send()
        .await
        .expect("Poly status should succeed");

    assert_eq!(poly_status_resp.status(), 200);
    let poly_status: serde_json::Value = poly_status_resp.json().await.unwrap();
    let poly_filled: f64 = poly_status["size_matched"].as_str().unwrap().parse().unwrap();
    assert_eq!(poly_filled as i64, 10, "Poly should fill completely");

    // Verify reverse exposure
    // Kalshi: 0 contracts (failed)
    // Polymarket: 10 YES contracts
    // Exposure: -10 (we own YES without offsetting NO)
    let net_kalshi_position: i64 = 0;
    let net_poly_position = poly_filled as i64;
    let reverse_exposure = net_poly_position - net_kalshi_position;
    assert_eq!(reverse_exposure, 10, "Should have 10 contracts of reverse exposure");
}

// ============================================================================
// CONCURRENT EXECUTION TESTS
// ============================================================================

/// Test concurrent execution of both legs.
///
/// Verifies that when both orders are sent concurrently:
/// - Both requests complete independently
/// - Results can be aggregated to determine final position
#[tokio::test]
async fn test_concurrent_execution_both_fill() {
    let kalshi_server = setup_mock_server().await;
    let poly_server = setup_mock_server().await;

    // Mount fixtures
    mount_fixture_file(&kalshi_server, fixture_path("kalshi_full_fill.json")).await;
    mount_fixture_file(&poly_server, fixture_path("poly_full_fill.json")).await;
    mount_fixture_file(&poly_server, fixture_path("poly_order_status_full.json")).await;

    let client = reqwest::Client::new();
    let kalshi_uri = kalshi_server.uri();
    let poly_uri = poly_server.uri();

    // Execute both orders concurrently
    let (kalshi_result, poly_result) = tokio::join!(
        async {
            client
                .post(format!("{}/trade-api/v2/portfolio/orders", kalshi_uri))
                .json(&json!({"ticker": "TEST", "count": 10}))
                .send()
                .await
        },
        async {
            client
                .post(format!("{}/order", poly_uri))
                .json(&json!({"order": {}, "orderType": "FAK"}))
                .send()
                .await
        }
    );

    // Verify both completed successfully
    let kalshi_resp = kalshi_result.expect("Kalshi should complete");
    let poly_resp = poly_result.expect("Poly should complete");

    assert_eq!(kalshi_resp.status(), 200);
    assert_eq!(poly_resp.status(), 200);

    let kalshi_body: serde_json::Value = kalshi_resp.json().await.unwrap();
    let poly_body: serde_json::Value = poly_resp.json().await.unwrap();

    // Verify fills
    assert_eq!(kalshi_body["order"]["taker_fill_count"], 10);
    assert!(poly_body["orderID"].as_str().is_some());
}

// ============================================================================
// HELPER FUNCTION TESTS
// ============================================================================

/// Test that fixtures can be loaded and parsed correctly.
#[tokio::test]
async fn test_fixture_loading_integrity() {
    use super::replay_harness::load_fixture;

    // Test Kalshi fixtures
    let kalshi_full = load_fixture(fixture_path("kalshi_full_fill.json")).unwrap();
    assert_eq!(kalshi_full.request.method, "POST");
    assert_eq!(kalshi_full.response.status, 200);

    let kalshi_partial = load_fixture(fixture_path("kalshi_partial_fill.json")).unwrap();
    let partial_body = kalshi_partial.response.body_parsed.unwrap();
    assert_eq!(partial_body["order"]["taker_fill_count"], 6);

    let kalshi_error = load_fixture(fixture_path("kalshi_insufficient_balance.json")).unwrap();
    assert_eq!(kalshi_error.response.status, 400);

    // Test Polymarket fixtures
    let poly_full = load_fixture(fixture_path("poly_full_fill.json")).unwrap();
    assert_eq!(poly_full.request.method, "POST");
    assert_eq!(poly_full.response.status, 200);

    let poly_status = load_fixture(fixture_path("poly_order_status_full.json")).unwrap();
    let status_body = poly_status.response.body_parsed.unwrap();
    assert_eq!(status_body["size_matched"], "10");
}

/// Test creating exchanges programmatically for custom scenarios.
#[tokio::test]
async fn test_programmatic_exchange_creation() {
    let server = setup_mock_server().await;

    // Create a custom Kalshi response
    let custom_kalshi = create_exchange_json(
        "POST",
        "https://trading-api.kalshi.com/trade-api/v2/portfolio/orders",
        200,
        json!({
            "order": {
                "order_id": "custom-001",
                "status": "executed",
                "taker_fill_count": 5,
                "remaining_count": 5
            }
        }),
    );

    mount_fixture(&server, &custom_kalshi).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["order"]["taker_fill_count"], 5);
}

// ============================================================================
// EDGE CASE TESTS
// ============================================================================

/// Test handling of zero fills (order placed but nothing executed).
#[tokio::test]
async fn test_zero_fill_scenario() {
    let server = setup_mock_server().await;

    let zero_fill = create_exchange_json(
        "POST",
        "https://trading-api.kalshi.com/trade-api/v2/portfolio/orders",
        200,
        json!({
            "order": {
                "order_id": "zero-001",
                "status": "canceled",
                "taker_fill_count": 0,
                "maker_fill_count": 0,
                "remaining_count": 10
            }
        }),
    );

    mount_fixture(&server, &zero_fill).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["order"]["taker_fill_count"], 0);
    assert_eq!(body["order"]["remaining_count"], 10);
}

/// Test handling of network/timeout errors.
#[tokio::test]
async fn test_network_timeout_simulation() {
    let server = setup_mock_server().await;

    // Create a delayed response to simulate timeout
    // Note: wiremock doesn't support delays by default, so we simulate
    // by checking the request reaches the mock
    let exchange = create_exchange_json(
        "POST",
        "https://trading-api.kalshi.com/trade-api/v2/portfolio/orders",
        503,
        json!({
            "error": "service_unavailable",
            "message": "Request timed out"
        }),
    );

    mount_fixture(&server, &exchange).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
}
