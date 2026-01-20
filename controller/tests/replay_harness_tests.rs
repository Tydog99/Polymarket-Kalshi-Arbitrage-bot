//! Integration tests for the replay harness.
//!
//! These tests verify that the replay harness can:
//! 1. Load captured fixtures from JSON files
//! 2. Mount them on a wiremock mock server
//! 3. Serve the expected responses to HTTP clients

mod integration;

use integration::replay_harness::{
    create_exchange, create_exchange_json, extract_path, load_fixture, mount_fixture,
    mount_fixture_file, mount_fixtures, setup_mock_server,
};

// ============================================================================
// Unit tests for helper functions
// ============================================================================

#[test]
fn test_extract_path_https() {
    let url = "https://api.kalshi.com/trade-api/v2/portfolio/orders";
    assert_eq!(extract_path(url), "/trade-api/v2/portfolio/orders");
}

#[test]
fn test_extract_path_http() {
    let url = "http://localhost:8080/api/orders";
    assert_eq!(extract_path(url), "/api/orders");
}

#[test]
fn test_extract_path_with_query() {
    let url = "https://api.example.com/orders?limit=10&offset=0";
    assert_eq!(extract_path(url), "/orders?limit=10&offset=0");
}

#[test]
fn test_extract_path_no_path() {
    let url = "https://api.example.com";
    assert_eq!(extract_path(url), "/");
}

// ============================================================================
// Fixture loading tests
// ============================================================================

#[test]
fn test_load_fixture_from_file() {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/integration/fixtures/sample_kalshi_order.json"
    );

    let exchange = load_fixture(fixture_path).expect("Should load fixture");

    assert_eq!(exchange.request.method, "POST");
    assert_eq!(
        exchange.request.url,
        "https://trading-api.kalshi.com/trade-api/v2/portfolio/orders"
    );
    assert_eq!(exchange.response.status, 200);
    assert!(exchange.response.body_raw.contains("test-order-001"));
    assert_eq!(exchange.sequence, 1);
    assert_eq!(exchange.latency_ms, 142);
}

#[test]
fn test_load_fixture_invalid_path() {
    let result = load_fixture("nonexistent/path/to/fixture.json");
    assert!(result.is_err());
}

#[test]
fn test_create_exchange_helper() {
    let exchange = create_exchange(
        "POST",
        "https://api.kalshi.com/orders",
        200,
        r#"{"order_id": "123"}"#,
    );

    assert_eq!(exchange.request.method, "POST");
    assert_eq!(exchange.request.url, "https://api.kalshi.com/orders");
    assert_eq!(exchange.response.status, 200);
    assert!(exchange.response.body_raw.contains("123"));
    assert!(exchange.response.body_parsed.is_some());
}

#[test]
fn test_create_exchange_json_helper() {
    let exchange = create_exchange_json(
        "GET",
        "https://api.example.com/data",
        200,
        serde_json::json!({"key": "value", "count": 42}),
    );

    assert_eq!(exchange.request.method, "GET");
    assert_eq!(exchange.response.status, 200);
    let parsed = exchange.response.body_parsed.unwrap();
    assert_eq!(parsed["key"], "value");
    assert_eq!(parsed["count"], 42);
}

// ============================================================================
// Mock server tests
// ============================================================================

#[tokio::test]
async fn test_setup_mock_server() {
    let server = setup_mock_server().await;
    // Server should have a valid URI
    assert!(server.uri().starts_with("http://"));
}

#[tokio::test]
async fn test_mount_fixture_and_request() {
    let server = setup_mock_server().await;

    let exchange = create_exchange(
        "POST",
        "https://api.kalshi.com/trade-api/v2/portfolio/orders",
        200,
        r#"{"order": {"order_id": "test-123", "status": "executed"}}"#,
    );

    mount_fixture(&server, &exchange).await;

    // Make a request to the mock server
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
        .json(&serde_json::json!({"ticker": "TEST"}))
        .send()
        .await
        .expect("Request should succeed");

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.expect("Should parse JSON");
    assert_eq!(body["order"]["order_id"], "test-123");
    assert_eq!(body["order"]["status"], "executed");
}

#[tokio::test]
async fn test_mount_fixture_from_file() {
    let server = setup_mock_server().await;

    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/integration/fixtures/sample_kalshi_order.json"
    );

    let exchange = mount_fixture_file(&server, fixture_path).await;

    // Verify the fixture was loaded correctly
    assert_eq!(exchange.request.method, "POST");
    assert_eq!(exchange.response.status, 200);

    // Make a request to the mock server
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/trade-api/v2/portfolio/orders",
            server.uri()
        ))
        .json(&serde_json::json!({"ticker": "TEST-MARKET-123"}))
        .send()
        .await
        .expect("Request should succeed");

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.expect("Should parse JSON");
    assert_eq!(body["order"]["order_id"], "test-order-001");
    assert_eq!(body["order"]["taker_fill_count"], 5);
}

#[tokio::test]
async fn test_mount_fixture_consumes_once() {
    let server = setup_mock_server().await;

    let exchange = create_exchange("POST", "https://api.kalshi.com/orders", 200, r#"{"success": true}"#);

    // Mount once
    mount_fixture(&server, &exchange).await;

    let client = reqwest::Client::new();

    // First request succeeds
    let resp1 = client
        .post(format!("{}/orders", server.uri()))
        .send()
        .await
        .expect("First request should succeed");
    assert_eq!(resp1.status(), 200);

    // Second request fails (no more mocks)
    let resp2 = client
        .post(format!("{}/orders", server.uri()))
        .send()
        .await
        .expect("Second request should complete");
    // wiremock returns 404 for unmatched requests
    assert_eq!(resp2.status(), 404);
}

#[tokio::test]
async fn test_mount_multiple_fixtures_sequential() {
    let server = setup_mock_server().await;

    let exchange1 = create_exchange("POST", "https://api.kalshi.com/orders", 200, r#"{"order": 1}"#);
    let exchange2 = create_exchange("POST", "https://api.kalshi.com/orders", 200, r#"{"order": 2}"#);

    // Mount both
    mount_fixture(&server, &exchange1).await;
    mount_fixture(&server, &exchange2).await;

    let client = reqwest::Client::new();

    // First request gets first fixture
    let resp1 = client
        .post(format!("{}/orders", server.uri()))
        .send()
        .await
        .expect("Should succeed");
    let body1: serde_json::Value = resp1.json().await.unwrap();
    assert_eq!(body1["order"], 1);

    // Second request gets second fixture
    let resp2 = client
        .post(format!("{}/orders", server.uri()))
        .send()
        .await
        .expect("Should succeed");
    let body2: serde_json::Value = resp2.json().await.unwrap();
    assert_eq!(body2["order"], 2);

    // Third request has no more fixtures
    let resp3 = client
        .post(format!("{}/orders", server.uri()))
        .send()
        .await
        .expect("Should complete");
    assert_eq!(resp3.status(), 404);
}

#[tokio::test]
async fn test_mount_fixtures_batch() {
    let server = setup_mock_server().await;

    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/integration/fixtures/sample_kalshi_order.json"
    );

    // Mount the same fixture twice
    let exchanges = mount_fixtures(&server, &[fixture_path, fixture_path]).await;

    assert_eq!(exchanges.len(), 2);

    let client = reqwest::Client::new();

    // Both requests should succeed
    for _ in 0..2 {
        let resp = client
            .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
            .send()
            .await
            .expect("Should succeed");
        assert_eq!(resp.status(), 200);
    }
}

#[tokio::test]
async fn test_different_methods_independent() {
    let server = setup_mock_server().await;

    let get_exchange = create_exchange("GET", "https://api.example.com/data", 200, r#"{"method": "get"}"#);
    let post_exchange = create_exchange("POST", "https://api.example.com/data", 201, r#"{"method": "post"}"#);

    mount_fixture(&server, &get_exchange).await;
    mount_fixture(&server, &post_exchange).await;

    let client = reqwest::Client::new();

    // GET request
    let get_resp = client
        .get(format!("{}/data", server.uri()))
        .send()
        .await
        .expect("GET should succeed");
    assert_eq!(get_resp.status(), 200);
    let get_body: serde_json::Value = get_resp.json().await.unwrap();
    assert_eq!(get_body["method"], "get");

    // POST request
    let post_resp = client
        .post(format!("{}/data", server.uri()))
        .send()
        .await
        .expect("POST should succeed");
    assert_eq!(post_resp.status(), 201);
    let post_body: serde_json::Value = post_resp.json().await.unwrap();
    assert_eq!(post_body["method"], "post");
}

#[tokio::test]
async fn test_error_response_replay() {
    let server = setup_mock_server().await;

    let error_exchange = create_exchange_json(
        "POST",
        "https://api.kalshi.com/orders",
        400,
        serde_json::json!({
            "error": "invalid_request",
            "message": "Insufficient balance"
        }),
    );

    mount_fixture(&server, &error_exchange).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/orders", server.uri()))
        .send()
        .await
        .expect("Request should complete");

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn test_partial_fill_scenario() {
    let server = setup_mock_server().await;

    // Simulate a partial fill response from Kalshi
    let partial_fill = create_exchange_json(
        "POST",
        "https://api.kalshi.com/trade-api/v2/portfolio/orders",
        200,
        serde_json::json!({
            "order": {
                "order_id": "partial-001",
                "status": "executed",
                "taker_fill_count": 3,  // Only 3 of 5 filled
                "remaining_count": 2
            }
        }),
    );

    mount_fixture(&server, &partial_fill).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/trade-api/v2/portfolio/orders",
            server.uri()
        ))
        .json(&serde_json::json!({
            "ticker": "TEST",
            "count": 5
        }))
        .send()
        .await
        .expect("Request should succeed");

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["order"]["taker_fill_count"], 3);
    assert_eq!(body["order"]["remaining_count"], 2);
}
