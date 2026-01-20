//! Test harness for replaying captured HTTP exchanges via wiremock.
//!
//! This module provides utilities to load captured HTTP fixtures (from the capture
//! middleware in `trading::capture`) and mount them on a wiremock mock server for
//! integration testing.
//!
//! # Example Usage
//!
//! ```rust,ignore
//! use crate::tests::integration::replay_harness::*;
//!
//! #[tokio::test]
//! async fn test_arb_execution_kalshi_partial_fill() {
//!     let mock_server = MockServer::start().await;
//!
//!     // Load fixtures in sequence
//!     mount_fixture(&mock_server, "fixtures/kalshi_full_fill.json").await;
//!     mount_fixture(&mock_server, "fixtures/poly_partial_fill.json").await;
//!
//!     // Create client pointing at mock
//!     let kalshi = KalshiClient::new_with_base_url(mock_server.uri());
//!
//!     // Run code under test
//!     let result = execute_leg(&kalshi, &order_req).await;
//!
//!     // Assert
//!     assert_eq!(result.filled_qty, 6);
//! }
//! ```

use std::collections::HashMap;
use std::path::Path;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Re-export the capture types from trading crate for convenience
pub use trading::capture::{CapturedExchange, CapturedRequest, CapturedResponse};

/// Extract the path portion from a full URL.
///
/// Given a URL like "https://api.kalshi.com/trade-api/v2/portfolio/orders",
/// returns "/trade-api/v2/portfolio/orders".
pub fn extract_path(url: &str) -> &str {
    url.find("://")
        .and_then(|i| url[i + 3..].find('/'))
        .map(|i| {
            let start = url.find("://").unwrap() + 3 + i;
            &url[start..]
        })
        .unwrap_or("/")
}

/// Load a captured exchange from a JSON fixture file.
///
/// # Arguments
///
/// * `path` - Path to the JSON fixture file
///
/// # Returns
///
/// The parsed `CapturedExchange` or an error if the file cannot be read or parsed.
///
/// # Example
///
/// ```rust,ignore
/// let exchange = load_fixture("fixtures/kalshi_order.json")?;
/// assert_eq!(exchange.request.method, "POST");
/// ```
pub fn load_fixture<P: AsRef<Path>>(path: P) -> Result<CapturedExchange, Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(path)?;
    let exchange: CapturedExchange = serde_json::from_str(&contents)?;
    Ok(exchange)
}

/// Mount a captured exchange onto a mock server.
///
/// This creates a wiremock mock that:
/// - Matches requests with the same HTTP method
/// - Matches requests to paths containing the captured path
/// - Returns the captured response (status, headers, body)
///
/// The mock uses `up_to_n_times(1)` so fixtures are consumed in order when
/// multiple requests are made to the same endpoint.
///
/// # Arguments
///
/// * `server` - The wiremock MockServer to mount the fixture on
/// * `exchange` - The captured exchange to replay
///
/// # Example
///
/// ```rust,ignore
/// let server = MockServer::start().await;
/// let exchange = load_fixture("fixtures/order.json")?;
/// mount_fixture(&server, &exchange).await;
///
/// // Now requests to server.uri() + path will return the captured response
/// ```
pub async fn mount_fixture(server: &MockServer, exchange: &CapturedExchange) {
    let path = extract_path(&exchange.request.url);
    let method_str = exchange.request.method.as_str();

    // Build response template with status and body
    let mut response = ResponseTemplate::new(exchange.response.status)
        .set_body_raw(exchange.response.body_raw.clone(), "application/json");

    // Add response headers (except content-length which wiremock handles)
    for (key, value) in &exchange.response.headers {
        if key.to_lowercase() != "content-length" && key.to_lowercase() != "transfer-encoding" {
            response = response.append_header(key.as_str(), value.as_str());
        }
    }

    // Create a regex pattern that matches paths containing the captured path
    // Use regex::escape to handle any special characters in the path
    let path_pattern = format!(".*{}.*", regex::escape(path));

    Mock::given(method(method_str))
        .and(path_regex(path_pattern))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Load a fixture file and mount it on the mock server.
///
/// This is a convenience function that combines `load_fixture` and `mount_fixture`.
///
/// # Arguments
///
/// * `server` - The wiremock MockServer to mount the fixture on
/// * `path` - Path to the JSON fixture file
///
/// # Returns
///
/// The loaded `CapturedExchange` (useful for assertions)
///
/// # Panics
///
/// Panics if the fixture file cannot be loaded or parsed.
pub async fn mount_fixture_file<P: AsRef<Path>>(server: &MockServer, path: P) -> CapturedExchange {
    let exchange = load_fixture(&path).unwrap_or_else(|e| {
        panic!("Failed to load fixture {:?}: {}", path.as_ref(), e)
    });
    mount_fixture(server, &exchange).await;
    exchange
}

/// Load and mount multiple fixtures in sequence.
///
/// Fixtures are mounted in order, and since each uses `up_to_n_times(1)`,
/// they will be consumed in the order they are mounted.
///
/// # Arguments
///
/// * `server` - The wiremock MockServer to mount fixtures on
/// * `paths` - Slice of paths to fixture files
///
/// # Returns
///
/// Vector of loaded `CapturedExchange`s in the same order as paths
///
/// # Panics
///
/// Panics if any fixture file cannot be loaded or parsed.
pub async fn mount_fixtures<P: AsRef<Path>>(server: &MockServer, paths: &[P]) -> Vec<CapturedExchange> {
    let mut exchanges = Vec::new();
    for path in paths {
        let exchange = mount_fixture_file(server, path).await;
        exchanges.push(exchange);
    }
    exchanges
}

/// Create a mock server ready for replay testing.
///
/// This is a thin wrapper around `MockServer::start()` for consistency.
pub async fn setup_mock_server() -> MockServer {
    MockServer::start().await
}

/// Create a `CapturedExchange` programmatically for testing.
///
/// This is useful for creating test fixtures in code rather than loading from files.
///
/// # Arguments
///
/// * `method` - HTTP method (GET, POST, etc.)
/// * `url` - Full request URL
/// * `status` - Response status code
/// * `body` - Response body as a string
///
/// # Returns
///
/// A `CapturedExchange` with the specified values and default timestamps/headers.
pub fn create_exchange(
    method: &str,
    url: &str,
    status: u16,
    body: &str,
) -> CapturedExchange {
    CapturedExchange {
        captured_at: "2026-01-19T00:00:00.000Z".to_string(),
        sequence: 1,
        latency_ms: 100,
        request: CapturedRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: {
                let mut h = HashMap::new();
                h.insert("content-type".to_string(), "application/json".to_string());
                h
            },
            body: None,
        },
        response: CapturedResponse {
            status,
            headers: {
                let mut h = HashMap::new();
                h.insert("content-type".to_string(), "application/json".to_string());
                h
            },
            body_raw: body.to_string(),
            body_parsed: serde_json::from_str(body).ok(),
        },
    }
}

/// Create a `CapturedExchange` with a JSON body programmatically.
///
/// # Arguments
///
/// * `method` - HTTP method (GET, POST, etc.)
/// * `url` - Full request URL
/// * `status` - Response status code
/// * `body` - Response body as a serde_json::Value
///
/// # Returns
///
/// A `CapturedExchange` with the specified values.
pub fn create_exchange_json(
    method: &str,
    url: &str,
    status: u16,
    body: serde_json::Value,
) -> CapturedExchange {
    let body_raw = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    CapturedExchange {
        captured_at: "2026-01-19T00:00:00.000Z".to_string(),
        sequence: 1,
        latency_ms: 100,
        request: CapturedRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: {
                let mut h = HashMap::new();
                h.insert("content-type".to_string(), "application/json".to_string());
                h
            },
            body: None,
        },
        response: CapturedResponse {
            status,
            headers: {
                let mut h = HashMap::new();
                h.insert("content-type".to_string(), "application/json".to_string());
                h
            },
            body_raw,
            body_parsed: Some(body),
        },
    }
}
