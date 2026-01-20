//! Kalshi API client for order execution.

use anyhow::Result;
use arrayvec::ArrayString;
use reqwest_middleware::ClientWithMiddleware;
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;

use super::config::{KalshiConfig, KALSHI_API_BASE, KALSHI_API_DELAY_MS};
use super::types::{KalshiOrderRequest, KalshiOrderResponse, KalshiPositionsResponse};
use crate::capture::build_client_with_capture;

/// Timeout for order requests (shorter than general API timeout)
const ORDER_TIMEOUT: Duration = Duration::from_secs(5);

/// Global order counter for unique client_order_id generation
static ORDER_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Kalshi REST API client for authenticated requests and order execution.
pub struct KalshiApiClient {
    http: ClientWithMiddleware,
    pub config: KalshiConfig,
    /// Base URL for API requests (e.g., "https://api.elections.kalshi.com/trade-api/v2")
    base_url: String,
}

impl KalshiApiClient {
    /// Create a new Kalshi API client with the given configuration.
    ///
    /// Uses the default base URL and enables capture middleware if `CAPTURE_DIR` is set.
    pub fn new(config: KalshiConfig) -> Self {
        Self::new_with_base_url(config, KALSHI_API_BASE)
    }

    /// Create a new Kalshi API client with a custom base URL.
    ///
    /// This is useful for testing against mock servers.
    /// Enables capture middleware if `CAPTURE_DIR` is set.
    pub fn new_with_base_url(config: KalshiConfig, base_url: &str) -> Self {
        let http = build_client_with_capture(
            reqwest::Client::builder().timeout(Duration::from_secs(10)),
        );
        Self {
            http,
            config,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Get the base URL for this client.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Generate a unique order ID using timestamp and counter.
    #[inline]
    fn next_order_id() -> ArrayString<24> {
        let counter = ORDER_COUNTER.fetch_add(1, Ordering::Relaxed);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut buf = ArrayString::<24>::new();
        let _ = write!(&mut buf, "a{}{}", ts, counter);
        buf
    }

    /// Generic authenticated GET request with retry on rate limit.
    #[allow(dead_code)]
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut retries = 0;
        const MAX_RETRIES: u32 = 5;

        loop {
            let url = format!("{}{}", self.base_url, path);
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            // Kalshi signature uses FULL path including /trade-api/v2 prefix
            // but WITHOUT query parameters
            let path_without_query = path.split('?').next().unwrap_or(path);
            let full_path = format!("/trade-api/v2{}", path_without_query);
            let signature = self
                .config
                .sign(&format!("{}GET{}", timestamp_ms, full_path))?;

            let resp = self
                .http
                .get(&url)
                .header("KALSHI-ACCESS-KEY", &self.config.api_key_id)
                .header("KALSHI-ACCESS-SIGNATURE", &signature)
                .header("KALSHI-ACCESS-TIMESTAMP", timestamp_ms.to_string())
                .send()
                .await?;

            let status = resp.status();

            // Handle rate limit with exponential backoff
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                retries += 1;
                if retries > MAX_RETRIES {
                    anyhow::bail!("Kalshi API rate limited after {} retries", MAX_RETRIES);
                }
                let backoff_ms = 2000 * (1 << retries); // 4s, 8s, 16s, 32s, 64s
                debug!(
                    "[KALSHI] Rate limited, backing off {}ms (retry {}/{})",
                    backoff_ms, retries, MAX_RETRIES
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                continue;
            }

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Kalshi API error {}: {}", status, body);
            }

            let data: T = resp.json().await?;
            tokio::time::sleep(Duration::from_millis(KALSHI_API_DELAY_MS)).await;
            return Ok(data);
        }
    }

    /// Generic authenticated POST request.
    async fn post<T: serde::de::DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // Kalshi signature uses FULL path including /trade-api/v2 prefix
        let full_path = format!("/trade-api/v2{}", path);
        let msg = format!("{}POST{}", timestamp_ms, full_path);
        let signature = self.config.sign(&msg)?;

        // Serialize body manually (reqwest_middleware doesn't expose .json())
        let body_json = serde_json::to_string(body)?;

        let resp = self
            .http
            .post(&url)
            .header("KALSHI-ACCESS-KEY", &self.config.api_key_id)
            .header("KALSHI-ACCESS-SIGNATURE", &signature)
            .header("KALSHI-ACCESS-TIMESTAMP", timestamp_ms.to_string())
            .header("Content-Type", "application/json")
            .timeout(ORDER_TIMEOUT)
            .body(body_json)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Kalshi API error {}: {}", status, body);
        }

        let data: T = resp.json().await?;
        Ok(data)
    }

    /// Create an order on Kalshi.
    pub async fn create_order(&self, order: &KalshiOrderRequest<'_>) -> Result<KalshiOrderResponse> {
        let path = "/portfolio/orders";
        self.post(path, order).await
    }

    /// Create an IOC buy order (convenience method).
    pub async fn buy_ioc(
        &self,
        ticker: &str,
        side: &str, // "yes" or "no"
        price_cents: i64,
        count: i64,
    ) -> Result<KalshiOrderResponse> {
        debug_assert!(!ticker.is_empty(), "ticker must not be empty");
        debug_assert!(
            (1..=99).contains(&price_cents),
            "price must be 1-99"
        );
        debug_assert!(count >= 1, "count must be >= 1");

        let side_static: &'static str = if side == "yes" { "yes" } else { "no" };
        let order_id = Self::next_order_id();
        let order = KalshiOrderRequest::ioc_buy(
            Cow::Borrowed(ticker),
            side_static,
            price_cents,
            count,
            Cow::Borrowed(&order_id),
        );
        debug!(
            "[KALSHI] IOC {} {} @{}c x{}",
            side, ticker, price_cents, count
        );

        let resp = self.create_order(&order).await?;
        debug!(
            "[KALSHI] {} filled={}",
            resp.order.status,
            resp.order.filled_count()
        );
        Ok(resp)
    }

    /// Create an IOC sell order (convenience method).
    pub async fn sell_ioc(
        &self,
        ticker: &str,
        side: &str, // "yes" or "no"
        price_cents: i64,
        count: i64,
    ) -> Result<KalshiOrderResponse> {
        debug_assert!(!ticker.is_empty(), "ticker must not be empty");
        debug_assert!(
            (1..=99).contains(&price_cents),
            "price must be 1-99"
        );
        debug_assert!(count >= 1, "count must be >= 1");

        let side_static: &'static str = if side == "yes" { "yes" } else { "no" };
        let order_id = Self::next_order_id();
        let order = KalshiOrderRequest::ioc_sell(
            Cow::Borrowed(ticker),
            side_static,
            price_cents,
            count,
            Cow::Borrowed(&order_id),
        );
        debug!(
            "[KALSHI] SELL {} {} @{}c x{}",
            side, ticker, price_cents, count
        );

        let resp = self.create_order(&order).await?;
        debug!(
            "[KALSHI] {} filled={}",
            resp.order.status,
            resp.order.filled_count()
        );
        Ok(resp)
    }

    /// Get all portfolio positions with non-zero holdings.
    pub async fn get_positions(&self) -> Result<KalshiPositionsResponse> {
        // Only return positions where we have contracts (count_filter=position)
        let path = "/portfolio/positions?count_filter=position&limit=100";
        self.get(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_request_serialization() {
        let req = KalshiOrderRequest::ioc_buy(
            std::borrow::Cow::Borrowed("TEST-TICKER"),
            "yes",
            50,
            10,
            std::borrow::Cow::Borrowed("order-123"),
        );
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("TEST-TICKER"));
        assert!(json.contains("\"yes_price\":50"));
    }

    #[test]
    fn test_next_order_id_unique() {
        let id1 = KalshiApiClient::next_order_id();
        let id2 = KalshiApiClient::next_order_id();
        assert_ne!(id1.as_str(), id2.as_str());
    }

    #[test]
    fn test_order_id_format() {
        let id = KalshiApiClient::next_order_id();
        // Should start with 'a' followed by timestamp and counter
        assert!(id.starts_with('a'));
        assert!(id.len() > 1);
    }

    #[test]
    fn test_base_url_default() {
        // Create a mock config (we can't use from_env without real credentials)
        // Just verify the constant is used
        assert!(KALSHI_API_BASE.contains("kalshi"));
        assert!(KALSHI_API_BASE.starts_with("https://"));
    }

    #[test]
    fn test_base_url_trailing_slash_trimmed() {
        // Verify trailing slash handling (can't test full client without credentials)
        let url_with_slash = "https://mock.example.com/api/";
        let trimmed = url_with_slash.trim_end_matches('/');
        assert_eq!(trimmed, "https://mock.example.com/api");
    }
}
