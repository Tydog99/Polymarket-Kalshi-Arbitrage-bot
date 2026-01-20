# Capture/Replay Testing System Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build HTTP capture middleware and replay test harness to validate trade execution with real API traffic.

**Architecture:** Reqwest middleware intercepts order-related HTTP traffic, writes JSON fixture files. Tests load fixtures into wiremock mock server, run actual client code against it.

**Tech Stack:** `reqwest-middleware` for capture, `wiremock` for replay, `serde_json` for fixtures.

---

## Task 1: Add Dependencies

**Files:**
- Modify: `trading/Cargo.toml`
- Modify: `controller/Cargo.toml`

**Step 1: Add reqwest-middleware to trading crate**

Add to `trading/Cargo.toml` under `[dependencies]`:

```toml
reqwest-middleware = "0.4"
async-trait = "0.1"
chrono = "0.4"
```

**Step 2: Add wiremock to controller dev-dependencies**

Add to `controller/Cargo.toml` under `[dev-dependencies]`:

```toml
wiremock = "0.6"
```

**Step 3: Verify dependencies resolve**

Run: `cargo check -p trading -p controller`
Expected: Compiles without errors

**Step 4: Commit**

```bash
git add trading/Cargo.toml controller/Cargo.toml
git commit -m "deps: add reqwest-middleware and wiremock for capture/replay"
```

---

## Task 2: Create CaptureMiddleware Module

**Files:**
- Create: `trading/src/capture.rs`
- Modify: `trading/src/lib.rs`

**Step 1: Create the capture module**

Create `trading/src/capture.rs`:

```rust
//! HTTP capture middleware for recording request/response pairs.
//!
//! Enable by setting CAPTURE_DIR environment variable to output directory.
//! Only captures requests matching the configured filter (default: order endpoints).

use async_trait::async_trait;
use chrono::Utc;
use reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next, Result as MiddlewareResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{debug, error, info};

/// Sequence counter for ordering captured files
static CAPTURE_SEQUENCE: AtomicU32 = AtomicU32::new(1);

/// Captured HTTP exchange (request + response)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedExchange {
    pub captured_at: String,
    pub sequence: u32,
    pub latency_ms: u64,
    pub request: CapturedRequest,
    pub response: CapturedResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedRequest {
    pub method: String,
    pub url: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedResponse {
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub body_raw: String,
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
    pub fn matches(&self, url: &str) -> bool {
        match self {
            CaptureFilter::PathContains(patterns) => {
                patterns.iter().any(|p| url.contains(p))
            }
            CaptureFilter::All => true,
        }
    }

    /// Parse from CAPTURE_FILTER env var
    pub fn from_env() -> Self {
        match std::env::var("CAPTURE_FILTER").ok().as_deref() {
            Some("all") => CaptureFilter::All,
            Some(patterns) => CaptureFilter::PathContains(
                patterns.split(',').map(|s| s.trim().to_string()).collect()
            ),
            None => CaptureFilter::default(),
        }
    }
}

/// Middleware that captures HTTP traffic to JSON files
pub struct CaptureMiddleware {
    output_dir: PathBuf,
    filter: CaptureFilter,
}

impl CaptureMiddleware {
    pub fn new(output_dir: PathBuf, filter: CaptureFilter) -> Self {
        // Create output directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            error!("[CAPTURE] Failed to create output directory {:?}: {}", output_dir, e);
        } else {
            info!("[CAPTURE] Capturing HTTP traffic to {:?}", output_dir);
        }
        Self { output_dir, filter }
    }

    /// Create from CAPTURE_DIR env var, returns None if not set
    pub fn from_env() -> Option<Self> {
        std::env::var("CAPTURE_DIR").ok().map(|dir| {
            Self::new(PathBuf::from(dir), CaptureFilter::from_env())
        })
    }

    fn should_capture(&self, url: &str) -> bool {
        self.filter.matches(url)
    }

    async fn save_capture(&self, exchange: &CapturedExchange) {
        let method = &exchange.request.method;
        let path_slug = exchange.request.url
            .split('/')
            .last()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .take(30)
            .collect::<String>();

        let filename = format!(
            "{:03}_{}_{}_{}.json",
            exchange.sequence,
            method,
            path_slug,
            exchange.captured_at.replace(':', "-").replace('.', "-")
        );
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

        if !self.should_capture(&url) {
            return next.run(req, extensions).await;
        }

        let method = req.method().to_string();
        let request_headers: std::collections::HashMap<String, String> = req
            .headers()
            .iter()
            .filter(|(k, _)| {
                // Skip sensitive headers
                let name = k.as_str().to_lowercase();
                !name.contains("authorization") && !name.contains("api-key") && !name.contains("secret")
            })
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        // Clone body if present (for capture)
        let body_bytes = req.body()
            .and_then(|b| b.as_bytes())
            .map(|b| b.to_vec());
        let request_body: Option<serde_json::Value> = body_bytes
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok());

        let start = std::time::Instant::now();
        let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);

        // Execute request
        let response = next.run(req, extensions).await?;

        let latency_ms = start.elapsed().as_millis() as u64;
        let status = response.status().as_u16();

        let response_headers: std::collections::HashMap<String, String> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        // We need to read the body, which consumes the response
        // Clone the response parts we need, read body, then reconstruct
        let body_bytes = response.bytes().await?;
        let body_raw = String::from_utf8_lossy(&body_bytes).to_string();
        let body_parsed: Option<serde_json::Value> = serde_json::from_slice(&body_bytes).ok();

        // Capture the exchange
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
                headers: response_headers,
                body_raw: body_raw.clone(),
                body_parsed,
            },
        };

        self.save_capture(&exchange).await;

        // Reconstruct response with the body we read
        let mut builder = http::Response::builder().status(status);
        for (k, v) in exchange.response.headers.iter() {
            if let Ok(name) = http::header::HeaderName::try_from(k.as_str()) {
                if let Ok(value) = http::header::HeaderValue::from_str(v) {
                    builder = builder.header(name, value);
                }
            }
        }
        let http_response = builder
            .body(body_bytes)
            .map_err(|e| reqwest_middleware::Error::Middleware(anyhow::anyhow!("Failed to rebuild response: {}", e)))?;

        Ok(Response::from(http_response))
    }
}

/// Build a reqwest client with optional capture middleware
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
    fn test_filter_default() {
        let filter = CaptureFilter::default();
        assert!(filter.matches("https://api.example.com/orders"));
        assert!(filter.matches("https://api.example.com/v2/portfolio/orders"));
        assert!(filter.matches("https://api.example.com/fills"));
        assert!(!filter.matches("https://api.example.com/markets"));
        assert!(!filter.matches("https://api.example.com/events"));
    }

    #[test]
    fn test_filter_all() {
        let filter = CaptureFilter::All;
        assert!(filter.matches("https://api.example.com/anything"));
    }

    #[test]
    fn test_filter_custom() {
        let filter = CaptureFilter::PathContains(vec!["/custom".to_string()]);
        assert!(filter.matches("https://api.example.com/custom/path"));
        assert!(!filter.matches("https://api.example.com/other"));
    }
}
```

**Step 2: Add capture module to lib.rs**

Modify `trading/src/lib.rs` to add the capture module:

```rust
//! Shared trading execution library for Kalshi and Polymarket.

pub mod capture;
pub mod execution;
pub mod kalshi;
pub mod polymarket;
```

**Step 3: Verify module compiles**

Run: `cargo check -p trading`
Expected: Compiles without errors

**Step 4: Run tests**

Run: `cargo test -p trading capture`
Expected: All tests pass

**Step 5: Commit**

```bash
git add trading/src/capture.rs trading/src/lib.rs
git commit -m "feat(trading): add HTTP capture middleware"
```

---

## Task 3: Make Kalshi Client Support Configurable Base URL and Middleware

**Files:**
- Modify: `controller/src/kalshi.rs`
- Modify: `controller/src/config.rs`

**Step 1: Add base_url field to KalshiConfig**

In `controller/src/config.rs`, find the `KALSHI_API_BASE` constant around line 10. We'll also add a helper function. Add after the existing constants:

```rust
/// Get Kalshi API base URL (allows override via env var for testing)
pub fn kalshi_api_base() -> String {
    std::env::var("KALSHI_API_BASE")
        .unwrap_or_else(|_| KALSHI_API_BASE.to_string())
}
```

**Step 2: Modify KalshiApiClient to use middleware-enabled client**

In `controller/src/kalshi.rs`, make these changes:

First, add the import at the top (around line 7):
```rust
use reqwest_middleware::ClientWithMiddleware;
```

Then modify the struct (around line 205-208):
```rust
pub struct KalshiApiClient {
    http: ClientWithMiddleware,
    pub config: KalshiConfig,
}
```

Then modify the `new` function (around line 210-218):
```rust
impl KalshiApiClient {
    pub fn new(config: KalshiConfig) -> Self {
        let base_builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(10));

        Self {
            http: trading::capture::build_client_with_capture(base_builder),
            config,
        }
    }
```

**Step 3: Update request methods to use configurable base URL**

Find the `get` method (around line 231) and update it to use the configurable base:

Change:
```rust
let url = format!("{}{}", KALSHI_API_BASE, path);
```

To:
```rust
let url = format!("{}{}", crate::config::kalshi_api_base(), path);
```

Do the same for the `post` method (around line 301):

Change:
```rust
let url = format!("{}{}", KALSHI_API_BASE, path);
```

To:
```rust
let url = format!("{}{}", crate::config::kalshi_api_base(), path);
```

**Step 4: Update imports**

Remove the `KALSHI_API_BASE` from the imports at line 24 if it's no longer directly used (it may still be needed elsewhere, check first).

**Step 5: Verify compilation**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 6: Run tests**

Run: `cargo test -p controller`
Expected: All existing tests pass

**Step 7: Commit**

```bash
git add controller/src/kalshi.rs controller/src/config.rs
git commit -m "feat(kalshi): support configurable base URL and capture middleware"
```

---

## Task 4: Make Polymarket Client Support Capture Middleware

**Files:**
- Modify: `controller/src/polymarket_clob.rs`

**Step 1: Add middleware import**

At the top of `controller/src/polymarket_clob.rs`, add:
```rust
use reqwest_middleware::ClientWithMiddleware;
```

**Step 2: Change http field type**

Find the `PolymarketAsyncClient` struct (around line 402-411) and change:
```rust
pub struct PolymarketAsyncClient {
    host: String,
    chain_id: u64,
    http: ClientWithMiddleware,  // Changed from reqwest::Client
    wallet: Arc<LocalWallet>,
    funder: String,
    wallet_address_str: String,
    address_header: HeaderValue,
}
```

**Step 3: Update the new function to use capture middleware**

Find the `new` function (around line 414-440) and update the client building:

```rust
pub fn new(host: &str, chain_id: u64, private_key: &str, funder: &str) -> Result<Self> {
    let wallet = private_key.parse::<LocalWallet>()?.with_chain_id(chain_id);
    let wallet_address_str = format!("{:?}", wallet.address());
    let address_header = HeaderValue::from_str(&wallet_address_str)
        .map_err(|e| anyhow!("Invalid wallet address for header: {}", e))?;

    // Build async client with connection pooling and keepalive
    let base_builder = reqwest::Client::builder()
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .tcp_nodelay(true)
        .timeout(std::time::Duration::from_secs(10));

    let http = trading::capture::build_client_with_capture(base_builder);

    Ok(Self {
        host: host.trim_end_matches('/').to_string(),
        chain_id,
        http,
        wallet: Arc::new(wallet),
        funder: funder.to_string(),
        wallet_address_str,
        address_header,
    })
}
```

**Step 4: Verify compilation**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 5: Commit**

```bash
git add controller/src/polymarket_clob.rs
git commit -m "feat(polymarket): support capture middleware"
```

---

## Task 5: Create Replay Test Harness

**Files:**
- Create: `controller/tests/integration/mod.rs`
- Create: `controller/tests/integration/replay_harness.rs`
- Create: `controller/tests/integration/fixtures/.gitkeep`

**Step 1: Create test directory structure**

```bash
mkdir -p controller/tests/integration/fixtures
touch controller/tests/integration/fixtures/.gitkeep
```

**Step 2: Create the replay harness module**

Create `controller/tests/integration/replay_harness.rs`:

```rust
//! Test harness for replaying captured HTTP exchanges via wiremock.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Captured HTTP exchange (mirrors trading::capture::CapturedExchange)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedExchange {
    pub captured_at: String,
    pub sequence: u32,
    pub latency_ms: u64,
    pub request: CapturedRequest,
    pub response: CapturedResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedRequest {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body_raw: String,
    pub body_parsed: Option<serde_json::Value>,
}

impl CapturedExchange {
    /// Load a captured exchange from a JSON file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let exchange: Self = serde_json::from_str(&contents)?;
        Ok(exchange)
    }

    /// Extract the path portion from the URL
    pub fn request_path(&self) -> &str {
        self.request.url
            .find("://")
            .and_then(|i| self.request.url[i + 3..].find('/'))
            .map(|i| {
                let start = self.request.url.find("://").unwrap() + 3 + i;
                &self.request.url[start..]
            })
            .unwrap_or("/")
    }
}

/// Mount a captured exchange onto a mock server
/// Uses up_to_n_times(1) so fixtures are consumed in order
pub async fn mount_fixture(server: &MockServer, exchange: &CapturedExchange) {
    let path = exchange.request_path();
    let method_str = exchange.request.method.as_str();

    // Build response template
    let mut response = ResponseTemplate::new(exchange.response.status)
        .set_body_raw(exchange.response.body_raw.clone(), "application/json");

    // Add response headers (except content-length which wiremock handles)
    for (key, value) in &exchange.response.headers {
        if key.to_lowercase() != "content-length" {
            response = response.append_header(key.as_str(), value.as_str());
        }
    }

    Mock::given(method(method_str))
        .and(path_regex(format!(".*{}.*", regex::escape(path))))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Load and mount multiple fixtures in sequence
pub async fn mount_fixtures<P: AsRef<Path>>(server: &MockServer, paths: &[P]) -> Vec<CapturedExchange> {
    let mut exchanges = Vec::new();
    for path in paths {
        match CapturedExchange::load(path) {
            Ok(exchange) => {
                mount_fixture(server, &exchange).await;
                exchanges.push(exchange);
            }
            Err(e) => {
                panic!("Failed to load fixture {:?}: {}", path.as_ref(), e);
            }
        }
    }
    exchanges
}

/// Test helper: create a mock server ready for replay testing
pub async fn setup_mock_server() -> MockServer {
    MockServer::start().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_path() {
        let exchange = CapturedExchange {
            captured_at: "2026-01-19T00:00:00Z".to_string(),
            sequence: 1,
            latency_ms: 100,
            request: CapturedRequest {
                method: "POST".to_string(),
                url: "https://api.kalshi.com/trade-api/v2/portfolio/orders".to_string(),
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

        assert_eq!(exchange.request_path(), "/trade-api/v2/portfolio/orders");
    }
}
```

**Step 3: Create the integration test module**

Create `controller/tests/integration/mod.rs`:

```rust
//! Integration tests using captured HTTP fixtures.

pub mod replay_harness;

pub use replay_harness::*;
```

**Step 4: Verify tests compile**

Run: `cargo test -p controller --test '*' -- --list 2>&1 | head -20`
Expected: Test discovery works without errors

**Step 5: Run harness tests**

Run: `cargo test -p controller replay_harness`
Expected: All tests pass

**Step 6: Commit**

```bash
git add controller/tests/
git commit -m "feat(tests): add replay test harness with wiremock"
```

---

## Task 6: Create Sample Fixture and First Integration Test

**Files:**
- Create: `controller/tests/integration/fixtures/sample_kalshi_order.json`
- Create: `controller/tests/integration/test_replay.rs`

**Step 1: Create a sample fixture**

Create `controller/tests/integration/fixtures/sample_kalshi_order.json`:

```json
{
  "captured_at": "2026-01-19T20:15:00.123Z",
  "sequence": 1,
  "latency_ms": 142,
  "request": {
    "method": "POST",
    "url": "https://api.elections.kalshi.com/trade-api/v2/portfolio/orders",
    "headers": {
      "content-type": "application/json"
    },
    "body": {
      "ticker": "TEST-MARKET-123",
      "side": "no",
      "action": "buy",
      "count": 5,
      "no_price": 45,
      "type": "limit",
      "time_in_force": "immediate_or_cancel"
    }
  },
  "response": {
    "status": 200,
    "headers": {
      "content-type": "application/json"
    },
    "body_raw": "{\"order\":{\"order_id\":\"test-order-001\",\"ticker\":\"TEST-MARKET-123\",\"side\":\"no\",\"action\":\"buy\",\"type\":\"limit\",\"no_price\":45,\"status\":\"executed\",\"taker_fill_count\":5,\"maker_fill_count\":0,\"taker_fill_cost\":225,\"maker_fill_cost\":0}}",
    "body_parsed": {
      "order": {
        "order_id": "test-order-001",
        "ticker": "TEST-MARKET-123",
        "side": "no",
        "action": "buy",
        "type": "limit",
        "no_price": 45,
        "status": "executed",
        "taker_fill_count": 5,
        "maker_fill_count": 0,
        "taker_fill_cost": 225,
        "maker_fill_cost": 0
      }
    }
  }
}
```

**Step 2: Create the integration test file**

Create `controller/tests/integration/test_replay.rs`:

```rust
//! Integration tests demonstrating fixture replay.

mod integration;

use integration::replay_harness::{CapturedExchange, mount_fixture, setup_mock_server};

#[tokio::test]
async fn test_fixture_loading() {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/integration/fixtures/sample_kalshi_order.json"
    );

    let exchange = CapturedExchange::load(fixture_path)
        .expect("Should load fixture");

    assert_eq!(exchange.request.method, "POST");
    assert_eq!(exchange.response.status, 200);
    assert!(exchange.response.body_raw.contains("test-order-001"));
}

#[tokio::test]
async fn test_mock_server_replay() {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/integration/fixtures/sample_kalshi_order.json"
    );

    let exchange = CapturedExchange::load(fixture_path)
        .expect("Should load fixture");

    let server = setup_mock_server().await;
    mount_fixture(&server, &exchange).await;

    // Make a request to the mock server
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
        .json(&serde_json::json!({
            "ticker": "TEST-MARKET-123",
            "side": "no",
            "action": "buy",
            "count": 5
        }))
        .send()
        .await
        .expect("Request should succeed");

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.expect("Should parse JSON");
    assert_eq!(body["order"]["order_id"], "test-order-001");
    assert_eq!(body["order"]["taker_fill_count"], 5);
}

#[tokio::test]
async fn test_sequential_fixtures() {
    // This test demonstrates that fixtures are consumed in order
    // (up_to_n_times(1) behavior)

    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/integration/fixtures/sample_kalshi_order.json"
    );

    let exchange = CapturedExchange::load(fixture_path)
        .expect("Should load fixture");

    let server = setup_mock_server().await;

    // Mount the same fixture twice
    mount_fixture(&server, &exchange).await;
    mount_fixture(&server, &exchange).await;

    let client = reqwest::Client::new();

    // First request uses first mount
    let resp1 = client
        .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("First request should succeed");
    assert_eq!(resp1.status(), 200);

    // Second request uses second mount
    let resp2 = client
        .post(format!("{}/trade-api/v2/portfolio/orders", server.uri()))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Second request should succeed");
    assert_eq!(resp2.status(), 200);

    // Third request has no mount - should fail or return 404
    // (wiremock returns 404 by default for unmatched requests)
}
```

**Step 3: Run integration tests**

Run: `cargo test -p controller test_replay`
Expected: All tests pass

**Step 4: Commit**

```bash
git add controller/tests/
git commit -m "feat(tests): add sample fixture and replay integration tests"
```

---

## Task 7: Document Usage

**Files:**
- Modify: `CLAUDE.md`

**Step 1: Add capture documentation**

Add the following section to `CLAUDE.md` after the "Environment Variables" section:

```markdown
## HTTP Traffic Capture (Testing)

Capture live HTTP traffic for replay testing:

```bash
# Capture to a directory (creates fixture files)
CAPTURE_DIR=fixtures/session_001 dotenvx run -- cargo run --release

# With manual trade CLI
CAPTURE_DIR=fixtures/kalshi_test cargo run --bin manual-trade -- ...

# Custom filter (default: /orders, /order, /fills)
CAPTURE_FILTER=all CAPTURE_DIR=fixtures/debug dotenvx run -- cargo run --release

# Override Kalshi API base URL (for testing against mock server)
KALSHI_API_BASE=http://localhost:8080 cargo test
```

**Capture file format:**
- One JSON file per request/response pair
- Files named: `{sequence}_{method}_{path}_{timestamp}.json`
- Sensitive headers (Authorization, API keys) are excluded

**Replay in tests:**
```rust
use crate::tests::integration::replay_harness::*;

let server = setup_mock_server().await;
mount_fixture(&server, &exchange).await;
// Point client at server.uri()
```
```

**Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add HTTP traffic capture documentation"
```

---

## Task 8: Final Verification

**Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests pass

**Step 2: Build release**

Run: `cargo build --release`
Expected: Builds without errors

**Step 3: Test capture manually**

Run:
```bash
mkdir -p /tmp/capture_test
CAPTURE_DIR=/tmp/capture_test DRY_RUN=1 cargo run -p controller --release 2>&1 | head -50
```

Expected: Should see "[CAPTURE] Capturing HTTP traffic to..." in logs

**Step 4: Verify capture creates files**

Run: `ls -la /tmp/capture_test/`
Expected: Should see capture files if any HTTP requests were made during startup

**Step 5: Final commit**

```bash
git add -A
git status
# If any uncommitted changes:
git commit -m "chore: final cleanup for capture/replay system"
```

---

## Summary

After completing all tasks, you will have:

1. **CaptureMiddleware** in `trading/src/capture.rs` - intercepts HTTP traffic, writes JSON fixtures
2. **Configurable base URLs** - `KALSHI_API_BASE` env var for testing against mock servers
3. **Middleware-enabled clients** - Both Kalshi and Polymarket clients support capture
4. **Replay harness** in `controller/tests/integration/` - loads fixtures into wiremock
5. **Sample fixture and tests** - demonstrates the full capture/replay workflow
6. **Documentation** in `CLAUDE.md` - usage instructions

**Next steps after this plan:**
- Merge PR #11 (manual trade CLI)
- Use manual trades with `CAPTURE_DIR` to capture real Kalshi scenarios
- Build out fixture library for all test scenarios
- Capture Polymarket scenarios
- Build combined scenario tests
