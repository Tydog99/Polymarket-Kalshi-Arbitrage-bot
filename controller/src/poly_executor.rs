//! Trait abstraction for Polymarket order execution.
//!
//! This module provides a `PolyExecutor` trait that abstracts Polymarket order execution,
//! enabling dependency injection for testing the ExecutionEngine with mock implementations.

use anyhow::Result;
use async_trait::async_trait;

use crate::polymarket_clob::PolyFillAsync;

// =============================================================================
// POLY EXECUTOR TRAIT
// =============================================================================

/// Trait for Polymarket order execution (enables mocking).
///
/// This trait abstracts the `buy_fak` and `sell_fak` methods from `SharedAsyncClient`,
/// allowing the `ExecutionEngine` to accept either a real client or a mock for testing.
#[async_trait]
pub trait PolyExecutor: Send + Sync {
    /// Execute a Fill-And-Kill buy order.
    ///
    /// # Arguments
    /// * `token_id` - The Polymarket token ID (YES or NO token)
    /// * `price` - Price as a decimal (0.0 to 1.0)
    /// * `size` - Number of contracts to buy
    ///
    /// # Returns
    /// `PolyFillAsync` with order_id, filled_size, and fill_cost
    async fn buy_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync>;

    /// Execute a Fill-And-Kill sell order.
    ///
    /// # Arguments
    /// * `token_id` - The Polymarket token ID (YES or NO token)
    /// * `price` - Price as a decimal (0.0 to 1.0)
    /// * `size` - Number of contracts to sell
    ///
    /// # Returns
    /// `PolyFillAsync` with order_id, filled_size, and fill_cost
    async fn sell_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync>;
}

// =============================================================================
// MOCK IMPLEMENTATION (FOR TESTING)
// =============================================================================

/// Mock module for testing. Available in all builds but only used in tests.
pub mod mock {
    use super::*;
    use anyhow::anyhow;
    use std::collections::HashMap;
    use std::sync::RwLock;

    /// Canned response configuration for mock client.
    #[derive(Clone, Debug)]
    pub enum MockResponse {
        /// Full fill at specified size and price
        FullFill { size: f64, price: f64 },
        /// Partial fill (filled less than requested)
        PartialFill { filled: f64, price: f64 },
        /// Order failed with error
        Error(String),
    }

    /// Record of a call to the mock client.
    #[derive(Clone, Debug)]
    pub struct MockCall {
        pub method: String,
        pub token_id: String,
        pub price: f64,
        pub size: f64,
    }

    /// Mock Polymarket client for testing.
    ///
    /// Allows configuring canned responses for specific token IDs.
    /// If no response is configured for a token, returns an error.
    /// Also tracks all calls for verification in tests.
    pub struct MockPolyClient {
        responses: RwLock<HashMap<String, MockResponse>>,
        calls: RwLock<Vec<MockCall>>,
    }

    impl MockPolyClient {
        /// Create a new mock client with no configured responses.
        pub fn new() -> Self {
            Self {
                responses: RwLock::new(HashMap::new()),
                calls: RwLock::new(Vec::new()),
            }
        }

        /// Get all calls made to this mock.
        pub fn get_calls(&self) -> Vec<MockCall> {
            self.calls.read().unwrap().clone()
        }

        /// Get all sell calls made to this mock.
        pub fn get_sell_calls(&self) -> Vec<MockCall> {
            self.calls.read().unwrap()
                .iter()
                .filter(|c| c.method == "sell_fak")
                .cloned()
                .collect()
        }

        /// Record a call to the mock.
        fn record_call(&self, method: &str, token_id: &str, price: f64, size: f64) {
            let mut calls = self.calls.write().unwrap();
            calls.push(MockCall {
                method: method.to_string(),
                token_id: token_id.to_string(),
                price,
                size,
            });
        }

        /// Configure a full fill response for a token.
        pub fn set_full_fill(&self, token: &str, size: f64, price: f64) {
            let mut responses = self.responses.write().unwrap();
            responses.insert(token.to_string(), MockResponse::FullFill { size, price });
        }

        /// Configure a partial fill response for a token.
        pub fn set_partial_fill(&self, token: &str, filled: f64, price: f64) {
            let mut responses = self.responses.write().unwrap();
            responses.insert(token.to_string(), MockResponse::PartialFill { filled, price });
        }

        /// Configure an error response for a token.
        pub fn set_error(&self, token: &str, error: &str) {
            let mut responses = self.responses.write().unwrap();
            responses.insert(token.to_string(), MockResponse::Error(error.to_string()));
        }

        /// Get the configured response for a token.
        fn get_response(&self, token_id: &str) -> Option<MockResponse> {
            let responses = self.responses.read().unwrap();
            responses.get(token_id).cloned()
        }
    }

    impl Default for MockPolyClient {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl PolyExecutor for MockPolyClient {
        async fn buy_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
            self.record_call("buy_fak", token_id, price, size);
            match self.get_response(token_id) {
                Some(MockResponse::FullFill { size, price }) => Ok(PolyFillAsync {
                    order_id: format!("mock-buy-{}", token_id),
                    filled_size: size,
                    fill_cost: size * price,
                }),
                Some(MockResponse::PartialFill { filled, price }) => Ok(PolyFillAsync {
                    order_id: format!("mock-buy-{}", token_id),
                    filled_size: filled,
                    fill_cost: filled * price,
                }),
                Some(MockResponse::Error(msg)) => Err(anyhow!("Mock error: {}", msg)),
                None => Err(anyhow!("No mock response configured for token: {}", token_id)),
            }
        }

        async fn sell_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
            self.record_call("sell_fak", token_id, price, size);
            match self.get_response(token_id) {
                Some(MockResponse::FullFill { size, price }) => Ok(PolyFillAsync {
                    order_id: format!("mock-sell-{}", token_id),
                    filled_size: size,
                    fill_cost: size * price,
                }),
                Some(MockResponse::PartialFill { filled, price }) => Ok(PolyFillAsync {
                    order_id: format!("mock-sell-{}", token_id),
                    filled_size: filled,
                    fill_cost: filled * price,
                }),
                Some(MockResponse::Error(msg)) => Err(anyhow!("Mock error: {}", msg)),
                None => Err(anyhow!("No mock response configured for token: {}", token_id)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::mock::MockPolyClient;

    #[tokio::test]
    async fn test_mock_full_fill() {
        let mock = MockPolyClient::new();
        mock.set_full_fill("token-yes", 10.0, 0.65);

        let result = mock.buy_fak("token-yes", 0.65, 10.0).await.unwrap();
        assert_eq!(result.filled_size, 10.0);
        assert!((result.fill_cost - 6.5).abs() < 0.001);
        assert!(result.order_id.contains("mock-buy"));
    }

    #[tokio::test]
    async fn test_mock_partial_fill() {
        let mock = MockPolyClient::new();
        mock.set_partial_fill("token-no", 5.0, 0.35);

        let result = mock.sell_fak("token-no", 0.35, 10.0).await.unwrap();
        assert_eq!(result.filled_size, 5.0);
        assert!((result.fill_cost - 1.75).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_mock_error() {
        let mock = MockPolyClient::new();
        mock.set_error("bad-token", "Insufficient balance");

        let result = mock.buy_fak("bad-token", 0.50, 10.0).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Insufficient balance"));
    }

    #[tokio::test]
    async fn test_mock_no_config() {
        let mock = MockPolyClient::new();

        let result = mock.buy_fak("unknown-token", 0.50, 10.0).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No mock response configured"));
    }
}
