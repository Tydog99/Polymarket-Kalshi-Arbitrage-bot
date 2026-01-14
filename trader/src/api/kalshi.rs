//! Kalshi API client implementation

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use rsa::{
    pkcs1::DecodeRsaPrivateKey,
    pkcs1v15::{SigningKey, VerifyingKey},
    signature::{RandomizedSigner, SignatureEncoding, Verifier},
    RsaPrivateKey, RsaPublicKey,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, warn};

const KALSHI_API_BASE: &str = "https://trading-api.kalshi.com/trade-api/v2";
const ORDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Kalshi API client configuration
pub struct KalshiConfig {
    pub api_key_id: String,
    pub private_key: RsaPrivateKey,
}

impl KalshiConfig {
    /// Create config from API key and private key PEM string
    pub fn new(api_key_id: String, private_key_pem: &str) -> Result<Self> {
        let private_key = RsaPrivateKey::from_pkcs1_pem(private_key_pem.trim())
            .context("Failed to parse private key PEM")?;
        Ok(Self {
            api_key_id,
            private_key,
        })
    }

    /// Sign a message using RSA-PSS with SHA-256
    pub fn sign(&self, message: &str) -> Result<String> {
        debug!("[KALSHI] Signing message: {}", message);
        let signing_key = SigningKey::<Sha256>::new(self.private_key.clone());
        let signature = signing_key.sign_with_rng(&mut rand::thread_rng(), message.as_bytes());
        let sig_b64 = general_purpose::STANDARD.encode(signature.to_bytes());
        Ok(sig_b64)
    }
}

/// Kalshi order request
#[derive(Debug, Serialize)]
struct KalshiOrderRequest<'a> {
    ticker: &'a str,
    action: &'a str, // "buy" or "sell"
    side: &'a str,   // "yes" or "no"
    count: i64,
    price: i64,
    #[serde(rename = "type")]
    order_type: &'a str, // "ioc" for immediate-or-cancel
    client_order_id: String,
}

/// Kalshi order response
#[derive(Debug, Deserialize)]
pub struct KalshiOrderResponse {
    pub order: KalshiOrder,
}

#[derive(Debug, Deserialize)]
pub struct KalshiOrder {
    pub order_id: String,
    pub ticker: String,
    pub side: String,
    pub action: String,
    pub count: i64,
    pub filled_count: i64,
    pub price: i64,
    #[serde(default)]
    pub taker_fill_cost: Option<i64>,
    #[serde(default)]
    pub maker_fill_cost: Option<i64>,
}

impl KalshiOrder {
    pub fn filled_count(&self) -> i64 {
        self.filled_count
    }
}

/// Kalshi API client
pub struct KalshiApiClient {
    http: reqwest::Client,
    config: KalshiConfig,
}

impl KalshiApiClient {
    /// Create a new Kalshi API client
    pub fn new(config: KalshiConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            config,
        }
    }

    /// Generate a unique client order ID
    fn next_order_id() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("a{}{}", ts, count)
    }

    /// Make an authenticated POST request
    async fn post<T: serde::de::DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = format!("{}{}", KALSHI_API_BASE, path);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let full_path = format!("/trade-api/v2{}", path);
        let msg = format!("{}POST{}", timestamp_ms, full_path);
        let signature = self.config.sign(&msg)?;

        let resp = self
            .http
            .post(&url)
            .header("KALSHI-ACCESS-KEY", &self.config.api_key_id)
            .header("KALSHI-ACCESS-SIGNATURE", &signature)
            .header("KALSHI-ACCESS-TIMESTAMP", timestamp_ms.to_string())
            .header("Content-Type", "application/json")
            .timeout(ORDER_TIMEOUT)
            .json(body)
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

    /// Create an IOC (Immediate-Or-Cancel) order
    pub async fn create_ioc_order(
        &self,
        ticker: &str,
        side: &str,
        action: &str,
        price: i64,
        count: i64,
    ) -> Result<KalshiOrderResponse> {
        let order = KalshiOrderRequest {
            ticker,
            action,
            side,
            count,
            price,
            order_type: "ioc",
            client_order_id: Self::next_order_id(),
        };

        self.post("/portfolio/orders", &order).await
    }

    /// Sell IOC order
    pub async fn sell_ioc(
        &self,
        ticker: &str,
        side: &str,
        price: i64,
        count: i64,
    ) -> Result<KalshiOrderResponse> {
        self.create_ioc_order(ticker, side, "sell", price, count)
            .await
    }

    /// Buy IOC order
    pub async fn buy_ioc(
        &self,
        ticker: &str,
        side: &str,
        price: i64,
        count: i64,
    ) -> Result<KalshiOrderResponse> {
        self.create_ioc_order(ticker, side, "buy", price, count)
            .await
    }
}
