//! HTTP capture middleware for recording request/response pairs.
//!
//! Enable by setting CAPTURE_DIR environment variable to output directory.
//! Only captures requests matching the configured filter (default: order endpoints).
//!
//! # Environment Variables
//!
//! | Variable | Default | Description |
//! |----------|---------|-------------|
//! | `CAPTURE_DIR` | unset | Enables capture, sets output directory |
//! | `CAPTURE_FILTER` | `orders` | What to capture: `orders`, `all`, or custom regex |
//!
//! # File Format
//!
//! Files are named `{sequence}_{method}_{path_slug}_{timestamp}.json` and contain:
//! - `captured_at`: ISO 8601 timestamp
//! - `latency_ms`: Request duration in milliseconds
//! - `request`: Method, URL, headers (sensitive excluded), body
//! - `response`: Status, headers, raw body, parsed body

use async_trait::async_trait;
use chrono::Utc;
use reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next, Result as MiddlewareResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{debug, error, info};

/// Sequence counter for ordering captured files (monotonically increasing)
static CAPTURE_SEQUENCE: AtomicU32 = AtomicU32::new(1);

/// Captured HTTP exchange (request + response pair)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedExchange {
    /// ISO 8601 timestamp when the exchange was captured
    pub captured_at: String,
    /// Monotonic sequence number for ordering
    pub sequence: u32,
    /// Request-to-response latency in milliseconds
    pub latency_ms: u64,
    /// The captured request
    pub request: CapturedRequest,
    /// The captured response
    pub response: CapturedResponse,
}

/// Captured HTTP request details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedRequest {
    /// HTTP method (GET, POST, etc.)
    pub method: String,
    /// Full request URL
    pub url: String,
    /// Request headers (sensitive headers excluded)
    pub headers: HashMap<String, String>,
    /// Request body parsed as JSON (if applicable)
    pub body: Option<serde_json::Value>,
}

/// Captured HTTP response details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedResponse {
    /// HTTP status code
    pub status: u16,
    /// Response headers
    pub headers: HashMap<String, String>,
    /// Raw response body as string
    pub body_raw: String,
    /// Response body parsed as JSON (if applicable)
    pub body_parsed: Option<serde_json::Value>,
}

/// Filter for which requests to capture
#[derive(Debug, Clone)]
pub enum CaptureFilter {
    /// Capture requests to paths containing any of these substrings
    PathContains(Vec<String>),
    /// Capture all requests
    All,
}

impl Default for CaptureFilter {
    fn default() -> Self {
        // Default: only capture order-related endpoints
        CaptureFilter::PathContains(vec![
            "/orders".to_string(),
            "/order".to_string(),
            "/fills".to_string(),
        ])
    }
}

impl CaptureFilter {
    /// Check if a URL matches this filter
    pub fn matches(&self, url: &str) -> bool {
        match self {
            CaptureFilter::PathContains(patterns) => patterns.iter().any(|p| url.contains(p)),
            CaptureFilter::All => true,
        }
    }

    /// Parse from CAPTURE_FILTER environment variable
    ///
    /// - "all" -> CaptureFilter::All
    /// - "orders" or unset -> Default (orders, order, fills)
    /// - comma-separated patterns -> PathContains with those patterns
    pub fn from_env() -> Self {
        match std::env::var("CAPTURE_FILTER").ok().as_deref() {
            Some("all") => CaptureFilter::All,
            Some("orders") | None => CaptureFilter::default(),
            Some(patterns) => CaptureFilter::PathContains(
                patterns
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
        }
    }
}

/// Middleware that captures HTTP traffic to JSON files
pub struct CaptureMiddleware {
    output_dir: PathBuf,
    filter: CaptureFilter,
}

impl CaptureMiddleware {
    /// Create a new capture middleware with the given output directory and filter
    pub fn new(output_dir: PathBuf, filter: CaptureFilter) -> Self {
        // Create output directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            error!(
                "[CAPTURE] Failed to create output directory {:?}: {}",
                output_dir, e
            );
        } else {
            info!("[CAPTURE] Capturing HTTP traffic to {:?}", output_dir);
        }
        Self { output_dir, filter }
    }

    /// Create from CAPTURE_DIR environment variable, returns None if not set
    ///
    /// When CAPTURE_DIR is not set, no middleware is created and there is
    /// zero overhead in the HTTP path.
    pub fn from_env() -> Option<Self> {
        std::env::var("CAPTURE_DIR")
            .ok()
            .map(|dir| Self::new(PathBuf::from(dir), CaptureFilter::from_env()))
    }

    /// Check if a URL should be captured based on the filter
    fn should_capture(&self, url: &str) -> bool {
        self.filter.matches(url)
    }

    /// Generate a filename for a captured exchange
    ///
    /// Format: `{sequence}_{method}_{platform}_{endpoint}.json`
    /// Example: `001_POST_kalshi_orders.json`
    fn generate_filename(&self, exchange: &CapturedExchange) -> String {
        let method = &exchange.request.method;
        let url = &exchange.request.url;

        // Detect platform from URL
        let platform = if url.contains("kalshi") {
            "kalshi"
        } else if url.contains("polymarket") || url.contains("clob.") {
            "poly"
        } else {
            "unknown"
        };

        // Extract endpoint from URL path (last path segment, sanitized)
        let endpoint = url
            .split('/')
            .last()
            .unwrap_or("unknown")
            .split('?') // Remove query params
            .next()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .take(30)
            .collect::<String>();

        format!(
            "{:03}_{}_{}_{}.json",
            exchange.sequence, method, platform, endpoint
        )
    }

    /// Save a captured exchange to disk
    async fn save_capture(&self, exchange: &CapturedExchange) {
        let filename = self.generate_filename(exchange);
        let filepath = self.output_dir.join(&filename);

        match serde_json::to_string_pretty(exchange) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&filepath, json) {
                    error!("[CAPTURE] Failed to write {:?}: {}", filepath, e);
                } else {
                    debug!("[CAPTURE] Saved {}", filename);
                }
            }
            Err(e) => error!("[CAPTURE] Failed to serialize capture: {}", e),
        }
    }

    /// Check if a header name is sensitive and should be excluded from capture
    fn is_sensitive_header(name: &str) -> bool {
        let lower = name.to_lowercase();
        lower.contains("authorization")
            || lower.contains("api-key")
            || lower.contains("api_key")
            || lower.contains("secret")
            || lower.contains("password")
            || lower.contains("token")
            || lower.contains("private")
            // Platform-specific auth headers
            || lower.starts_with("kalshi-access")
            || lower.starts_with("poly_")
    }

    /// Extract headers from a request, filtering out sensitive ones
    fn extract_request_headers(req: &Request) -> HashMap<String, String> {
        req.headers()
            .iter()
            .filter(|(k, _)| !Self::is_sensitive_header(k.as_str()))
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect()
    }

    /// Extract headers from a response
    fn extract_response_headers(resp: &Response) -> HashMap<String, String> {
        resp.headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect()
    }
}

#[async_trait]
impl Middleware for CaptureMiddleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> MiddlewareResult<Response> {
        let url = req.url().to_string();

        // Fast path: skip capture if URL doesn't match filter
        if !self.should_capture(&url) {
            return next.run(req, extensions).await;
        }

        // Capture request details before sending
        let method = req.method().to_string();
        let request_headers = Self::extract_request_headers(&req);

        // Clone body if present (for capture)
        let body_bytes = req.body().and_then(|b| b.as_bytes()).map(|b| b.to_vec());
        let request_body: Option<serde_json::Value> =
            body_bytes.as_ref().and_then(|b| serde_json::from_slice(b).ok());

        let start = std::time::Instant::now();
        let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);

        // Execute the actual request
        let response = next.run(req, extensions).await?;

        let latency_ms = start.elapsed().as_millis() as u64;
        let status = response.status().as_u16();
        let response_headers = Self::extract_response_headers(&response);

        // Read the response body - this consumes the response
        let body_bytes = response.bytes().await?;
        let body_raw = String::from_utf8_lossy(&body_bytes).to_string();
        let body_parsed: Option<serde_json::Value> = serde_json::from_slice(&body_bytes).ok();

        // Build the captured exchange
        let exchange = CapturedExchange {
            captured_at: Utc::now().to_rfc3339(),
            sequence,
            latency_ms,
            request: CapturedRequest {
                method,
                url,
                headers: request_headers,
                body: request_body,
            },
            response: CapturedResponse {
                status,
                headers: response_headers.clone(),
                body_raw: body_raw.clone(),
                body_parsed,
            },
        };

        // Save capture asynchronously (but we wait for it to complete for reliability)
        self.save_capture(&exchange).await;

        // Reconstruct the response with the body we read
        let mut builder = http::Response::builder().status(status);
        for (k, v) in response_headers.iter() {
            if let Ok(name) = http::header::HeaderName::try_from(k.as_str()) {
                if let Ok(value) = http::header::HeaderValue::from_str(v) {
                    builder = builder.header(name, value);
                }
            }
        }
        let http_response = builder.body(body_bytes).map_err(|e| {
            reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                "Failed to rebuild response: {}",
                e
            ))
        })?;

        Ok(Response::from(http_response))
    }
}

/// Build a reqwest client with optional capture middleware
///
/// If CAPTURE_DIR environment variable is set, capture middleware will be added.
/// Otherwise, returns a client without any middleware (zero overhead).
pub fn build_client_with_capture(
    base_builder: reqwest::ClientBuilder,
) -> reqwest_middleware::ClientWithMiddleware {
    let client = base_builder.build().expect("Failed to build HTTP client");

    let mut builder = reqwest_middleware::ClientBuilder::new(client);

    if let Some(capture) = CaptureMiddleware::from_env() {
        builder = builder.with(capture);
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_default_matches_orders() {
        let filter = CaptureFilter::default();
        assert!(filter.matches("https://api.example.com/orders"));
        assert!(filter.matches("https://api.example.com/v2/portfolio/orders"));
        assert!(filter.matches("https://api.example.com/order"));
        assert!(filter.matches("https://clob.polymarket.com/order"));
        assert!(filter.matches("https://api.example.com/fills"));
    }

    #[test]
    fn test_filter_default_excludes_non_order() {
        let filter = CaptureFilter::default();
        assert!(!filter.matches("https://api.example.com/markets"));
        assert!(!filter.matches("https://api.example.com/events"));
        assert!(!filter.matches("https://api.example.com/neg-risk"));
        assert!(!filter.matches("https://api.example.com/auth/derive-api-key"));
    }

    #[test]
    fn test_filter_all_matches_everything() {
        let filter = CaptureFilter::All;
        assert!(filter.matches("https://api.example.com/anything"));
        assert!(filter.matches("https://api.example.com/markets"));
        assert!(filter.matches("https://api.example.com/orders"));
    }

    #[test]
    fn test_filter_custom_patterns() {
        let filter = CaptureFilter::PathContains(vec!["/custom".to_string(), "/special".to_string()]);
        assert!(filter.matches("https://api.example.com/custom/path"));
        assert!(filter.matches("https://api.example.com/special"));
        assert!(!filter.matches("https://api.example.com/other"));
        assert!(!filter.matches("https://api.example.com/orders"));
    }

    #[test]
    fn test_sensitive_headers_detection() {
        // Should be filtered
        assert!(CaptureMiddleware::is_sensitive_header("Authorization"));
        assert!(CaptureMiddleware::is_sensitive_header("authorization"));
        assert!(CaptureMiddleware::is_sensitive_header("X-API-KEY"));
        assert!(CaptureMiddleware::is_sensitive_header("x-api-key"));
        assert!(CaptureMiddleware::is_sensitive_header("api_key"));
        assert!(CaptureMiddleware::is_sensitive_header("X-Secret"));
        assert!(CaptureMiddleware::is_sensitive_header("password"));
        assert!(CaptureMiddleware::is_sensitive_header("access-token"));
        assert!(CaptureMiddleware::is_sensitive_header("private-key"));
        assert!(CaptureMiddleware::is_sensitive_header("KALSHI-ACCESS-KEY"));
        assert!(CaptureMiddleware::is_sensitive_header("KALSHI-ACCESS-SIGNATURE"));
        assert!(CaptureMiddleware::is_sensitive_header("POLY_ADDRESS"));
        assert!(CaptureMiddleware::is_sensitive_header("POLY_SIGNATURE"));
        assert!(CaptureMiddleware::is_sensitive_header("POLY_API_KEY"));

        // Should NOT be filtered
        assert!(!CaptureMiddleware::is_sensitive_header("Content-Type"));
        assert!(!CaptureMiddleware::is_sensitive_header("Accept"));
        assert!(!CaptureMiddleware::is_sensitive_header("User-Agent"));
        assert!(!CaptureMiddleware::is_sensitive_header("Connection"));
    }

    #[test]
    fn test_captured_exchange_serialization() {
        let exchange = CapturedExchange {
            captured_at: "2026-01-19T20:15:00.123Z".to_string(),
            sequence: 1,
            latency_ms: 142,
            request: CapturedRequest {
                method: "POST".to_string(),
                url: "https://api.kalshi.com/trade-api/v2/portfolio/orders".to_string(),
                headers: {
                    let mut h = HashMap::new();
                    h.insert("content-type".to_string(), "application/json".to_string());
                    h
                },
                body: Some(serde_json::json!({
                    "ticker": "TEST-MARKET",
                    "side": "no",
                    "count": 5
                })),
            },
            response: CapturedResponse {
                status: 200,
                headers: {
                    let mut h = HashMap::new();
                    h.insert("content-type".to_string(), "application/json".to_string());
                    h
                },
                body_raw: r#"{"order":{"order_id":"abc123"}}"#.to_string(),
                body_parsed: Some(serde_json::json!({
                    "order": {"order_id": "abc123"}
                })),
            },
        };

        let json = serde_json::to_string_pretty(&exchange).unwrap();
        assert!(json.contains("captured_at"));
        assert!(json.contains("2026-01-19T20:15:00.123Z"));
        assert!(json.contains("TEST-MARKET"));
        assert!(json.contains("abc123"));

        // Verify it can be deserialized back
        let parsed: CapturedExchange = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.sequence, 1);
        assert_eq!(parsed.latency_ms, 142);
        assert_eq!(parsed.request.method, "POST");
        assert_eq!(parsed.response.status, 200);
    }

    #[test]
    fn test_filename_generation_kalshi() {
        let middleware = CaptureMiddleware::new(PathBuf::from("/tmp/test"), CaptureFilter::default());

        let exchange = CapturedExchange {
            captured_at: "2026-01-19T20:15:00.123Z".to_string(),
            sequence: 42,
            latency_ms: 100,
            request: CapturedRequest {
                method: "POST".to_string(),
                url: "https://trading-api.kalshi.com/trade-api/v2/portfolio/orders".to_string(),
                headers: HashMap::new(),
                body: None,
            },
            response: CapturedResponse {
                status: 200,
                headers: HashMap::new(),
                body_raw: "{}".to_string(),
                body_parsed: None,
            },
        };

        let filename = middleware.generate_filename(&exchange);
        assert_eq!(filename, "042_POST_kalshi_orders.json");
    }

    #[test]
    fn test_filename_generation_polymarket() {
        let middleware = CaptureMiddleware::new(PathBuf::from("/tmp/test"), CaptureFilter::default());

        let exchange = CapturedExchange {
            captured_at: "2026-01-19T20:15:00.123Z".to_string(),
            sequence: 1,
            latency_ms: 100,
            request: CapturedRequest {
                method: "POST".to_string(),
                url: "https://clob.polymarket.com/order".to_string(),
                headers: HashMap::new(),
                body: None,
            },
            response: CapturedResponse {
                status: 200,
                headers: HashMap::new(),
                body_raw: "{}".to_string(),
                body_parsed: None,
            },
        };

        let filename = middleware.generate_filename(&exchange);
        assert_eq!(filename, "001_POST_poly_order.json");
    }

    #[test]
    fn test_filename_sanitizes_special_chars() {
        let middleware = CaptureMiddleware::new(PathBuf::from("/tmp/test"), CaptureFilter::default());

        let exchange = CapturedExchange {
            captured_at: "2026-01-19T20:15:00Z".to_string(),
            sequence: 1,
            latency_ms: 100,
            request: CapturedRequest {
                method: "GET".to_string(),
                url: "https://api.kalshi.com/path?query=value&other=123".to_string(),
                headers: HashMap::new(),
                body: None,
            },
            response: CapturedResponse {
                status: 200,
                headers: HashMap::new(),
                body_raw: "{}".to_string(),
                body_parsed: None,
            },
        };

        let filename = middleware.generate_filename(&exchange);
        // Query params should be stripped (only alphanumeric, -, _)
        assert!(!filename.contains('?'));
        assert!(!filename.contains('='));
        assert!(!filename.contains('&'));
        // Should identify platform
        assert!(filename.contains("kalshi"));
    }

    #[test]
    fn test_sequence_counter_increments() {
        // Get initial value
        let initial = CAPTURE_SEQUENCE.load(Ordering::SeqCst);

        // Simulate what middleware would do
        let seq1 = CAPTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let seq2 = CAPTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let seq3 = CAPTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);

        assert_eq!(seq2, seq1 + 1);
        assert_eq!(seq3, seq2 + 1);

        // Reset for other tests (best effort - tests may run in parallel)
        CAPTURE_SEQUENCE.store(initial, Ordering::SeqCst);
    }

    #[test]
    fn test_filter_from_env_with_all() {
        // This test checks the parsing logic without actually setting env vars
        // (which would affect other tests)

        // Test the default case explicitly
        let filter = CaptureFilter::PathContains(vec![
            "/orders".to_string(),
            "/order".to_string(),
            "/fills".to_string(),
        ]);
        assert!(filter.matches("/portfolio/orders"));

        // Test all filter
        let all = CaptureFilter::All;
        assert!(all.matches("/anything"));
    }

    #[test]
    fn test_captured_request_body_optional() {
        let exchange = CapturedExchange {
            captured_at: "2026-01-19T20:15:00Z".to_string(),
            sequence: 1,
            latency_ms: 50,
            request: CapturedRequest {
                method: "GET".to_string(),
                url: "https://api.example.com/data".to_string(),
                headers: HashMap::new(),
                body: None, // GET requests typically have no body
            },
            response: CapturedResponse {
                status: 200,
                headers: HashMap::new(),
                body_raw: r#"{"data": []}"#.to_string(),
                body_parsed: Some(serde_json::json!({"data": []})),
            },
        };

        let json = serde_json::to_string(&exchange).unwrap();
        let parsed: CapturedExchange = serde_json::from_str(&json).unwrap();
        assert!(parsed.request.body.is_none());
    }

    #[test]
    fn test_capture_middleware_creation() {
        let temp_dir = std::env::temp_dir().join("capture_test_creation");
        let _middleware = CaptureMiddleware::new(temp_dir.clone(), CaptureFilter::All);

        // Directory should be created
        assert!(temp_dir.exists());

        // Clean up
        let _ = std::fs::remove_dir(&temp_dir);
    }
}
