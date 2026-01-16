//! Polymarket API client implementation

use anyhow::{Context, Result};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::Address;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::RwLock;
use tracing::{debug, warn};

/// Polymarket fill result
#[derive(Debug, Clone)]
pub struct PolyFillAsync {
    pub order_id: String,
    pub filled_size: f64,
    pub fill_cost: f64,
}

/// Prepared credentials for Polymarket API
pub struct PreparedCreds {
    pub api_key: String,
    #[allow(dead_code)] // Stored for future request signing
    pub api_secret: String,
}

impl PreparedCreds {
    /// Create credentials from API key and secret
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
        }
    }
}

/// Polymarket async client
pub struct PolymarketAsyncClient {
    http: reqwest::Client,
    host: String,
    #[allow(dead_code)] // Stored for future request signing
    wallet: LocalWallet,
    #[allow(dead_code)] // Stored for future request signing
    wallet_address: Address,
    #[allow(dead_code)] // Stored for future request signing
    funder: String,
    #[allow(dead_code)] // Stored for future request signing
    chain_id: u64,
}

impl PolymarketAsyncClient {
    /// Create a new Polymarket async client
    pub fn new(
        host: &str,
        chain_id: u64,
        private_key: &str,
        funder: &str,
    ) -> Result<Self> {
        let wallet = LocalWallet::from_str(private_key.trim_start_matches("0x"))
            .context("Failed to parse private key")?;
        let wallet_address = wallet.address();

        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            host: host.to_string(),
            wallet,
            wallet_address,
            funder: funder.to_string(),
            chain_id,
        })
    }

    /// Derive API key from wallet (simplified - in production this would use the actual API)
    #[allow(dead_code)]
    pub async fn derive_api_key(&self, _nonce: u64) -> Result<(String, String)> {
        // In a real implementation, this would call Polymarket's API key derivation endpoint
        // For now, we'll use a placeholder that requires the API key to be provided
        anyhow::bail!("API key derivation not implemented - provide POLYMARKET_API_KEY and POLYMARKET_API_SECRET in environment")
    }

    /// Build L2 headers for authenticated requests
    fn build_l2_headers(
        &self,
        _method: &str,
        _path: &str,
        _body: Option<&str>,
        creds: &PreparedCreds,
    ) -> Result<reqwest::header::HeaderMap> {
        use reqwest::header::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert("X-API-KEY", HeaderValue::from_str(&creds.api_key)?);
        // In production, would add signature here
        Ok(headers)
    }

    /// Post an order
    async fn post_order_async(
        &self,
        body: &str,
        creds: &PreparedCreds,
    ) -> Result<reqwest::Response> {
        let path = "/order";
        let headers = self.build_l2_headers("POST", path, Some(body), creds)?;

        let url = format!("{}{}", self.host, path);
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await?;

        Ok(resp)
    }

    /// Get order by ID
    pub async fn get_order_async(
        &self,
        order_id: &str,
        creds: &PreparedCreds,
    ) -> Result<PolymarketOrderResponse> {
        let path = format!("/data/order/{}", order_id);
        let url = format!("{}{}", self.host, path);
        let headers = self.build_l2_headers("GET", &path, None, creds)?;

        let resp = self
            .http
            .get(&url)
            .headers(headers)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("get_order failed {}: {}", status, body));
        }

        Ok(resp.json().await?)
    }

    /// Check neg_risk for token
    pub async fn check_neg_risk(&self, token_id: &str) -> Result<bool> {
        let url = format!("{}/neg-risk?token_id={}", self.host, token_id);
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", "remote-trader/1.0")
            .send()
            .await?;

        let val: serde_json::Value = resp.json().await?;
        Ok(val["neg_risk"].as_bool().unwrap_or(false))
    }
}

/// Polymarket order response
#[derive(Debug, Deserialize)]
pub struct PolymarketOrderResponse {
    #[serde(rename = "orderID")]
    pub order_id: String,
    #[serde(rename = "sizeMatched")]
    pub size_matched: String,
    pub price: String,
    pub status: String,
}

/// Shared async client wrapper
pub struct SharedAsyncClient {
    inner: std::sync::Arc<PolymarketAsyncClient>,
    creds: PreparedCreds,
    #[allow(dead_code)] // Stored for future request signing
    chain_id: u64,
    neg_risk_cache: RwLock<HashMap<String, bool>>,
}

impl SharedAsyncClient {
    /// Create a new shared async client
    pub fn new(
        client: PolymarketAsyncClient,
        creds: PreparedCreds,
        chain_id: u64,
    ) -> Self {
        Self {
            inner: std::sync::Arc::new(client),
            creds,
            chain_id,
            neg_risk_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Load neg_risk cache from JSON file
    #[allow(dead_code)] // Optional optimization hook (used by some deployments)
    pub fn load_cache(&self, path: &str) -> Result<usize> {
        match std::fs::read_to_string(path) {
            Ok(data) => {
                let map: HashMap<String, bool> = serde_json::from_str(&data)?;
                let count = map.len();
                let mut cache = self.neg_risk_cache.write().unwrap();
                *cache = map;
                Ok(count)
            }
            Err(_) => {
                warn!("[POLY] Cache file not found: {}", path);
                Ok(0)
            }
        }
    }

    /// Execute FAK buy order
    pub async fn buy_fak(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
    ) -> Result<PolyFillAsync> {
        debug_assert!(!token_id.is_empty(), "token_id must not be empty");
        debug_assert!(price > 0.0 && price < 1.0, "price must be 0 < p < 1");
        debug_assert!(size >= 1.0, "size must be >= 1");
        self.execute_order(token_id, price, size, "BUY").await
    }

    /// Execute FAK sell order
    pub async fn sell_fak(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
    ) -> Result<PolyFillAsync> {
        debug_assert!(!token_id.is_empty(), "token_id must not be empty");
        debug_assert!(price > 0.0 && price < 1.0, "price must be 0 < p < 1");
        debug_assert!(size >= 1.0, "size must be >= 1");
        self.execute_order(token_id, price, size, "SELL").await
    }

    async fn execute_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: &str,
    ) -> Result<PolyFillAsync> {
        // Check neg_risk cache first
        let neg_risk = {
            let cache = self.neg_risk_cache.read().unwrap();
            cache.get(token_id).copied()
        };

        let neg_risk = match neg_risk {
            Some(nr) => nr,
            None => {
                let nr = self.inner.check_neg_risk(token_id).await?;
                let mut cache = self.neg_risk_cache.write().unwrap();
                cache.insert(token_id.to_string(), nr);
                nr
            }
        };

        // Build order request (simplified - in production would build proper signed order)
        let order_body = serde_json::json!({
            "token_id": token_id,
            "price": price.to_string(),
            "size": size.to_string(),
            "side": side,
            "order_type": "FAK",
            "neg_risk": neg_risk,
        });

        let body_str = serde_json::to_string(&order_body)?;

        // Post order
        let resp = self.inner.post_order_async(&body_str, &self.creds).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Polymarket order failed {}: {}", status, body));
        }

        let resp_json: serde_json::Value = resp.json().await?;
        let order_id = resp_json["orderID"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        // Query fill status
        let order_info = self.inner.get_order_async(&order_id, &self.creds).await?;
        let filled_size: f64 = order_info.size_matched.parse().unwrap_or(0.0);
        let order_price: f64 = order_info.price.parse().unwrap_or(price);

        debug!(
            "[POLY-ASYNC] FAK {} {}: status={}, filled={:.2}/{:.2}, price={:.4}",
            side, order_info.order_id, order_info.status, filled_size, size, order_price
        );

        Ok(PolyFillAsync {
            order_id,
            filled_size,
            fill_cost: filled_size * order_price,
        })
    }
}
