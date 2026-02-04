//! Integration tests for Kalshi balance API.
//!
//! Tests the `get_balance()` method on `KalshiApiClient` using wiremock mock servers.

use rsa::RsaPrivateKey;
use serde_json::json;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use arb_bot::kalshi::{KalshiApiClient, KalshiConfig};

// ============================================================================
// TEST HELPERS
// ============================================================================

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

// ============================================================================
// TEST: GET BALANCE
// ============================================================================

/// Test get_balance returns correct balance information.
///
/// Scenario:
/// - Mock /portfolio/balance endpoint returns balance data
/// - Call get_balance()
/// - Verify response fields match expected values
#[tokio::test]
async fn test_kalshi_get_balance() {
    let server = MockServer::start().await;

    // Mock the /portfolio/balance endpoint
    let balance_response = json!({
        "balance": 150000,
        "portfolio_value": 25000,
        "updated_ts": 1706900000000_i64
    });

    Mock::given(method("GET"))
        .and(path_regex(".*portfolio/balance.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&balance_response))
        .expect(1)
        .mount(&server)
        .await;

    // Create test client
    let client = create_test_kalshi_client(&server);

    // Call get_balance
    let result = client.get_balance().await;

    // Verify success
    assert!(result.is_ok(), "get_balance should succeed: {:?}", result.err());

    let balance = result.unwrap();
    assert_eq!(balance.balance, 150000, "balance should be 150000 cents");
    assert_eq!(balance.portfolio_value, 25000, "portfolio_value should be 25000 cents");
    assert_eq!(balance.updated_ts, Some(1706900000000), "updated_ts should match");
}

/// Test get_balance handles missing optional field.
#[tokio::test]
async fn test_kalshi_get_balance_missing_updated_ts() {
    let server = MockServer::start().await;

    // Mock response without updated_ts
    let balance_response = json!({
        "balance": 100000,
        "portfolio_value": 0
    });

    Mock::given(method("GET"))
        .and(path_regex(".*portfolio/balance.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&balance_response))
        .expect(1)
        .mount(&server)
        .await;

    let client = create_test_kalshi_client(&server);
    let result = client.get_balance().await;

    assert!(result.is_ok(), "get_balance should handle missing optional fields");
    let balance = result.unwrap();
    assert_eq!(balance.balance, 100000);
    assert_eq!(balance.portfolio_value, 0);
    assert!(balance.updated_ts.is_none(), "updated_ts should be None when not provided");
}

/// Test get_balance handles API errors.
#[tokio::test]
async fn test_kalshi_get_balance_api_error() {
    let server = MockServer::start().await;

    // Mock an error response
    Mock::given(method("GET"))
        .and(path_regex(".*portfolio/balance.*"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .expect(1)
        .mount(&server)
        .await;

    let client = create_test_kalshi_client(&server);
    let result = client.get_balance().await;

    assert!(result.is_err(), "get_balance should return error on 401");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("401"),
        "Error should contain status code: {}",
        err
    );
}
