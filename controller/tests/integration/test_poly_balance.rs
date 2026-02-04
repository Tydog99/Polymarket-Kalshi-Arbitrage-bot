//! Tests for Polymarket balance fetching endpoint.
//!
//! These tests verify that `SharedAsyncClient::get_balance()` correctly:
//! 1. Makes authenticated GET requests to /balance-allowance endpoint
//! 2. Parses the response (balance and allowance in 6-decimal USDC format)
//! 3. Converts balance to cents correctly

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use arb_bot::polymarket_clob::{
    ApiCreds, PolymarketAsyncClient, PreparedCreds, SharedAsyncClient,
};

// Test private key (DO NOT use in production - this is a well-known test key)
const TEST_PRIVATE_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TEST_FUNDER: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

/// Helper to create a SharedAsyncClient pointing at a mock server.
fn create_test_client(server: &MockServer) -> SharedAsyncClient {
    let client = PolymarketAsyncClient::new(
        &server.uri(),
        137,
        TEST_PRIVATE_KEY,
        TEST_FUNDER,
        0, // EOA signature type
    )
    .expect("should create client");

    let api_creds = ApiCreds {
        api_key: "test_key".to_string(),
        api_secret: "dGVzdF9zZWNyZXQ=".to_string(), // base64 encoded "test_secret"
        api_passphrase: "test_pass".to_string(),
    };
    let creds = PreparedCreds::from_api_creds(&api_creds).expect("should create creds");

    SharedAsyncClient::new(client, creds, 137)
}

// ============================================================================
// TEST: GET BALANCE SUCCESS
// ============================================================================

/// Test get_balance returns correct balance and allowance from API response.
///
/// Scenario:
/// - API returns balance of 1500000000 (1500 USDC = 150000 cents)
/// - API returns allowance of 2000000000 (2000 USDC = 200000 cents)
/// - Expected: PolyBalance with correct values and cents conversion
#[tokio::test]
async fn test_poly_get_balance_success() {
    let server = MockServer::start().await;

    // Mock the /balance-allowance endpoint
    Mock::given(method("GET"))
        .and(path("/balance-allowance"))
        .and(query_param("asset_type", "COLLATERAL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "balance": "1500000000",
            "allowance": "2000000000"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = create_test_client(&server);

    // Call get_balance
    let balance = client.get_balance().await.expect("should get balance");

    // Verify the response fields
    assert_eq!(balance.balance, "1500000000", "balance string should match");
    assert_eq!(balance.allowance, "2000000000", "allowance string should match");

    // Verify cents conversion: 1500000000 / 10000 = 150000 cents ($1500.00)
    assert_eq!(
        balance.balance_as_cents(),
        150000,
        "balance should convert to 150000 cents"
    );
    assert_eq!(
        balance.allowance_as_cents(),
        200000,
        "allowance should convert to 200000 cents"
    );
}

// ============================================================================
// TEST: GET BALANCE ZERO
// ============================================================================

/// Test get_balance handles zero balance correctly.
#[tokio::test]
async fn test_poly_get_balance_zero() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/balance-allowance"))
        .and(query_param("asset_type", "COLLATERAL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "balance": "0",
            "allowance": "0"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = create_test_client(&server);

    let balance = client.get_balance().await.expect("should get balance");

    assert_eq!(balance.balance_as_cents(), 0);
    assert_eq!(balance.allowance_as_cents(), 0);
}

// ============================================================================
// TEST: GET BALANCE SMALL AMOUNT
// ============================================================================

/// Test get_balance handles small amounts (less than 1 cent).
#[tokio::test]
async fn test_poly_get_balance_small_amount() {
    let server = MockServer::start().await;

    // 5000 = 0.005 USDC = 0.5 cents (rounds to 0)
    Mock::given(method("GET"))
        .and(path("/balance-allowance"))
        .and(query_param("asset_type", "COLLATERAL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "balance": "5000",
            "allowance": "5000"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = create_test_client(&server);

    let balance = client.get_balance().await.expect("should get balance");

    // 5000 / 10000 = 0 cents (integer division)
    assert_eq!(balance.balance_as_cents(), 0);
}

// ============================================================================
// TEST: GET BALANCE API ERROR
// ============================================================================

/// Test get_balance handles API errors correctly.
#[tokio::test]
async fn test_poly_get_balance_api_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/balance-allowance"))
        .and(query_param("asset_type", "COLLATERAL"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .expect(1)
        .mount(&server)
        .await;

    let client = create_test_client(&server);

    let result = client.get_balance().await;

    assert!(result.is_err(), "should return error on 401");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("401") || err.contains("Unauthorized"),
        "error should mention 401: {}",
        err
    );
}
