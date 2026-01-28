//! Polymarket CLOB async client for order execution.
//!
//! This module provides:
//! - `PolymarketAsyncClient` - Low-level async HTTP client with EIP-712 signing
//! - `SharedAsyncClient` - Higher-level client with credential caching and order execution

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip712::{Eip712, TypedData};
use ethers::types::{H256, U256};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest_middleware::ClientWithMiddleware;
use serde::Serialize;
use serde_json::json;
use tokio::time::{sleep, Duration};

use super::types::{
    get_order_amounts_buy, get_order_amounts_sell, price_to_bps, price_valid, size_to_micro,
    ApiCreds, PolyFillAsync, PolyOrderType, PolymarketOrderResponse, PreparedCreds,
};
use crate::capture::build_client_with_capture;

// ============================================================================
// CONSTANTS
// ============================================================================

const USER_AGENT: &str = "py_clob_client";
const MSG_TO_SIGN: &str = "This message attests that I control the given wallet";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
// Signature type meanings (per Polymarket CLOB / py-clob-client conventions):
// - 0: EOA (standard private-key wallet like MetaMask)
// - 1/2: proxy / delegated signing modes
const SIGNATURE_TYPE_EOA: i32 = 0;

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

fn add_default_headers(headers: &mut HeaderMap) {
    headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
}

#[inline(always)]
fn current_unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[inline(always)]
fn generate_seed() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % u128::from(u32::MAX)
}

// ============================================================================
// EIP-712 SIGNING
// ============================================================================

fn clob_auth_digest(
    chain_id: u64,
    address_str: &str,
    timestamp: u64,
    nonce: u64,
) -> Result<H256> {
    let typed_json = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"}
            ],
            "ClobAuth": [
                {"name": "address", "type": "address"},
                {"name": "timestamp", "type": "string"},
                {"name": "nonce", "type": "uint256"},
                {"name": "message", "type": "string"}
            ]
        },
        "primaryType": "ClobAuth",
        "domain": { "name": "ClobAuthDomain", "version": "1", "chainId": chain_id },
        "message": { "address": address_str, "timestamp": timestamp.to_string(), "nonce": nonce, "message": MSG_TO_SIGN }
    });
    let typed: TypedData = serde_json::from_value(typed_json)?;
    Ok(typed.encode_eip712()?.into())
}

fn get_exchange_address(chain_id: u64, neg_risk: bool) -> Result<String> {
    match (chain_id, neg_risk) {
        (137, true) => Ok("0xC5d563A36AE78145C45a50134d48A1215220f80a".into()),
        (137, false) => Ok("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".into()),
        (80002, true) => Ok("0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296".into()),
        (80002, false) => Ok("0xdFE02Eb6733538f8Ea35D585af8DE5958AD99E40".into()),
        _ => Err(anyhow!("unsupported chain")),
    }
}

/// Order data for EIP712 signing (references to avoid clones in hot path).
struct OrderData<'a> {
    maker: &'a str,
    taker: &'a str,
    token_id: &'a str,
    maker_amount: &'a str,
    taker_amount: &'a str,
    side: i32,
    fee_rate_bps: &'a str,
    nonce: &'a str,
    signer: &'a str,
    expiration: &'a str,
    signature_type: i32,
    salt: u128,
}

fn order_typed_data(chain_id: u64, exchange: &str, data: &OrderData<'_>) -> Result<TypedData> {
    let typed_json = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "Order": [
                {"name":"salt","type":"uint256"},
                {"name":"maker","type":"address"},
                {"name":"signer","type":"address"},
                {"name":"taker","type":"address"},
                {"name":"tokenId","type":"uint256"},
                {"name":"makerAmount","type":"uint256"},
                {"name":"takerAmount","type":"uint256"},
                {"name":"expiration","type":"uint256"},
                {"name":"nonce","type":"uint256"},
                {"name":"feeRateBps","type":"uint256"},
                {"name":"side","type":"uint8"},
                {"name":"signatureType","type":"uint8"}
            ]
        },
        "primaryType": "Order",
        "domain": { "name": "Polymarket CTF Exchange", "version": "1", "chainId": chain_id, "verifyingContract": exchange },
        "message": {
            "salt": U256::from(data.salt),
            "maker": data.maker,
            "signer": data.signer,
            "taker": data.taker,
            "tokenId": U256::from_dec_str(data.token_id)?,
            "makerAmount": U256::from_dec_str(data.maker_amount)?,
            "takerAmount": U256::from_dec_str(data.taker_amount)?,
            "expiration": U256::from_dec_str(data.expiration)?,
            "nonce": U256::from_dec_str(data.nonce)?,
            "feeRateBps": U256::from_dec_str(data.fee_rate_bps)?,
            "side": data.side,
            "signatureType": data.signature_type,
        }
    });
    Ok(serde_json::from_value(typed_json)?)
}

// ============================================================================
// ORDER STRUCTURES
// ============================================================================

/// Order arguments for creating a new order.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OrderArgs {
    pub token_id: String,
    pub price: f64,
    pub size: f64,
    pub side: String,
    pub fee_rate_bps: Option<i64>,
    pub nonce: Option<i64>,
    pub expiration: Option<String>,
    pub taker: Option<String>,
}

/// Order structure for serialization.
#[derive(Debug, Clone, Serialize)]
pub struct OrderStruct {
    pub salt: u128,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: String,
    pub side: i32,
    #[serde(rename = "signatureType")]
    pub signature_type: i32,
}

/// Signed order ready for submission.
#[derive(Debug, Clone, Serialize)]
pub struct SignedOrder {
    pub order: OrderStruct,
    pub signature: String,
}

impl SignedOrder {
    /// Build the POST body for order submission.
    pub fn post_body(&self, owner: &str, order_type: &str) -> String {
        let side_str = if self.order.side == 0 { "BUY" } else { "SELL" };
        let mut buf = String::with_capacity(512);
        buf.push_str(r#"{"order":{"salt":"#);
        buf.push_str(&self.order.salt.to_string());
        buf.push_str(r#","maker":""#);
        buf.push_str(&self.order.maker);
        buf.push_str(r#"","signer":""#);
        buf.push_str(&self.order.signer);
        buf.push_str(r#"","taker":""#);
        buf.push_str(&self.order.taker);
        buf.push_str(r#"","tokenId":""#);
        buf.push_str(&self.order.token_id);
        buf.push_str(r#"","makerAmount":""#);
        buf.push_str(&self.order.maker_amount);
        buf.push_str(r#"","takerAmount":""#);
        buf.push_str(&self.order.taker_amount);
        buf.push_str(r#"","expiration":""#);
        buf.push_str(&self.order.expiration);
        buf.push_str(r#"","nonce":""#);
        buf.push_str(&self.order.nonce);
        buf.push_str(r#"","feeRateBps":""#);
        buf.push_str(&self.order.fee_rate_bps);
        buf.push_str(r#"","side":""#);
        buf.push_str(side_str);
        buf.push_str(r#"","signatureType":"#);
        buf.push_str(&self.order.signature_type.to_string());
        buf.push_str(r#","signature":""#);
        buf.push_str(&self.signature);
        buf.push_str(r#""},"owner":""#);
        buf.push_str(owner);
        buf.push_str(r#"","orderType":""#);
        buf.push_str(order_type);
        buf.push_str(r#""}"#);
        buf
    }
}

// ============================================================================
// ASYNC CLIENT
// ============================================================================

/// Default Polymarket CLOB API host
pub const POLYMARKET_CLOB_HOST: &str = "https://clob.polymarket.com";

/// Async Polymarket client for execution.
///
/// Handles L1/L2 authentication and HTTP requests to the CLOB API.
pub struct PolymarketAsyncClient {
    host: String,
    chain_id: u64,
    http: ClientWithMiddleware,
    wallet: Arc<LocalWallet>,
    funder: String,
    /// Signature type used in signed orders (0=EOA, 1/2=proxy modes).
    signature_type: i32,
    wallet_address_str: String,
    address_header: HeaderValue,
}

impl PolymarketAsyncClient {
    /// Create a new async client.
    ///
    /// # Arguments
    /// * `host` - CLOB API host (e.g., "https://clob.polymarket.com")
    /// * `chain_id` - Polygon chain ID (137 for mainnet, 80002 for Amoy testnet)
    /// * `private_key` - 0x-prefixed private key
    /// * `funder` - Wallet address that funds trades
    ///
    /// Enables capture middleware if `CAPTURE_DIR` is set.
    pub fn new(host: &str, chain_id: u64, private_key: &str, funder: &str) -> Result<Self> {
        Self::new_with_signature_type(host, chain_id, private_key, funder, SIGNATURE_TYPE_EOA)
    }

    /// Create a new async client with an explicit `signature_type`.
    ///
    /// Use this when your **signer address** (private key) differs from the **funder/maker**
    /// address shown in the Polymarket UI (proxy/delegated accounts).
    pub fn new_with_signature_type(
        host: &str,
        chain_id: u64,
        private_key: &str,
        funder: &str,
        signature_type: i32,
    ) -> Result<Self> {
        let wallet = private_key.parse::<LocalWallet>()?.with_chain_id(chain_id);
        let wallet_address_str = format!("{:?}", wallet.address());
        let address_header = HeaderValue::from_str(&wallet_address_str)
            .map_err(|e| anyhow!("Invalid wallet address for header: {}", e))?;

        // Build async client with connection pooling, keepalive, and optional capture middleware
        let http = build_client_with_capture(
            reqwest::Client::builder()
                .pool_max_idle_per_host(10)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .tcp_keepalive(std::time::Duration::from_secs(30))
                .tcp_nodelay(true)
                .timeout(std::time::Duration::from_secs(10)),
        );

        Ok(Self {
            host: host.trim_end_matches('/').to_string(),
            chain_id,
            http,
            wallet: Arc::new(wallet),
            funder: funder.to_string(),
            signature_type,
            wallet_address_str,
            address_header,
        })
    }

    /// Get the host URL for this client.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Build L1 headers for authentication (derive-api-key).
    ///
    /// wallet.sign_hash() is CPU-bound (~1ms), safe to call in async context.
    fn build_l1_headers(&self, nonce: u64) -> Result<HeaderMap> {
        let timestamp = current_unix_ts();
        let digest = clob_auth_digest(self.chain_id, &self.wallet_address_str, timestamp, nonce)?;
        let sig = self.wallet.sign_hash(digest)?;
        let mut headers = HeaderMap::new();
        headers.insert("POLY_ADDRESS", self.address_header.clone());
        headers.insert(
            "POLY_SIGNATURE",
            HeaderValue::from_str(&format!("0x{}", sig))?,
        );
        headers.insert(
            "POLY_TIMESTAMP",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("POLY_NONCE", HeaderValue::from_str(&nonce.to_string())?);
        add_default_headers(&mut headers);
        Ok(headers)
    }

    /// Derive API credentials from L1 wallet signature.
    pub async fn derive_api_key(&self, nonce: u64) -> Result<ApiCreds> {
        let url = format!("{}/auth/derive-api-key", self.host);
        let headers = self.build_l1_headers(nonce)?;
        let resp = self.http.get(&url).headers(headers).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("derive-api-key failed: {} {}", status, body));
        }
        Ok(resp.json().await?)
    }

    /// Build L2 headers for authenticated requests.
    fn build_l2_headers(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        creds: &PreparedCreds,
    ) -> Result<HeaderMap> {
        let timestamp = current_unix_ts();
        let mut message = format!("{}{}{}", timestamp, method, path);
        if let Some(b) = body {
            message.push_str(b);
        }

        let sig_b64 = creds.sign_b64(message.as_bytes());

        let mut headers = HeaderMap::with_capacity(9);
        headers.insert("POLY_ADDRESS", self.address_header.clone());
        headers.insert("POLY_SIGNATURE", HeaderValue::from_str(&sig_b64)?);
        headers.insert(
            "POLY_TIMESTAMP",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("POLY_API_KEY", creds.api_key_header());
        headers.insert("POLY_PASSPHRASE", creds.passphrase_header());
        add_default_headers(&mut headers);
        Ok(headers)
    }

    /// Post order to CLOB.
    pub async fn post_order_async(
        &self,
        body: String,
        creds: &PreparedCreds,
    ) -> Result<reqwest::Response> {
        let path = "/order";
        let url = format!("{}{}", self.host, path);
        let headers = self.build_l2_headers("POST", path, Some(&body), creds)?;

        let resp = self.http.post(&url).headers(headers).body(body).send().await?;

        Ok(resp)
    }

    /// Get order by ID.
    pub async fn get_order_async(
        &self,
        order_id: &str,
        creds: &PreparedCreds,
    ) -> Result<PolymarketOrderResponse> {
        let path = format!("/data/order/{}", order_id);
        let url = format!("{}{}", self.host, path);
        let headers = self.build_l2_headers("GET", &path, None, creds)?;

        let resp = self.http.get(&url).headers(headers).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_order failed {}: {}", status, body));
        }

        // Polymarket sometimes returns JSON `null` briefly right after placing an order.
        // Treat that as "not ready yet" so the caller can retry.
        let body = resp.text().await.unwrap_or_default();
        let trimmed = body.trim();
        if trimmed.eq_ignore_ascii_case("null") {
            return Err(anyhow!("get_order returned null (order not ready yet)"));
        }

        Ok(serde_json::from_str::<PolymarketOrderResponse>(trimmed)?)
    }

    /// Check neg_risk for token.
    pub async fn check_neg_risk(&self, token_id: &str) -> Result<bool> {
        let url = format!("{}/neg-risk?token_id={}", self.host, token_id);
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?;

        let val: serde_json::Value = resp.json().await?;
        Ok(val["neg_risk"].as_bool().unwrap_or(false))
    }

    /// Get wallet address as string.
    #[allow(dead_code)]
    pub fn wallet_address(&self) -> &str {
        &self.wallet_address_str
    }

    /// Get funder address.
    #[allow(dead_code)]
    pub fn funder(&self) -> &str {
        &self.funder
    }

    /// Get signature type (0=EOA, 1/2=proxy modes).
    #[allow(dead_code)]
    pub fn signature_type(&self) -> i32 {
        self.signature_type
    }

    /// Get reference to wallet.
    #[allow(dead_code)]
    pub fn wallet(&self) -> &LocalWallet {
        &self.wallet
    }
}

// ============================================================================
// SHARED ASYNC CLIENT
// ============================================================================

/// Shared async client wrapper for use in execution engine.
///
/// This provides a higher-level interface with:
/// - Pre-cached credentials
/// - Neg-risk caching
/// - FAK order execution methods
pub struct SharedAsyncClient {
    inner: Arc<PolymarketAsyncClient>,
    creds: PreparedCreds,
    chain_id: u64,
    /// Pre-cached neg_risk lookups.
    neg_risk_cache: std::sync::RwLock<HashMap<String, bool>>,
}

impl SharedAsyncClient {
    /// Create a new shared client.
    pub fn new(client: PolymarketAsyncClient, creds: PreparedCreds, chain_id: u64) -> Self {
        Self {
            inner: Arc::new(client),
            creds,
            chain_id,
            neg_risk_cache: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Load neg_risk cache from JSON file (output of build_sports_cache.py).
    pub fn load_cache(&self, path: &str) -> Result<usize> {
        let data = std::fs::read_to_string(path)?;
        let map: HashMap<String, bool> = serde_json::from_str(&data)?;
        let count = map.len();
        let mut cache = self.neg_risk_cache.write().unwrap();
        *cache = map;
        Ok(count)
    }

    /// Execute FAK buy order.
    pub async fn buy_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
        debug_assert!(!token_id.is_empty(), "token_id must not be empty");
        debug_assert!(price > 0.0 && price < 1.0, "price must be 0 < p < 1");
        debug_assert!(size >= 1.0, "size must be >= 1");
        self.execute_order(token_id, price, size, "BUY").await
    }

    /// Execute FAK sell order.
    pub async fn sell_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
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

        // Build signed order
        let signed = self.build_signed_order(token_id, price, size, side, neg_risk)?;
        // Owner must be the API key (not wallet address or funder!)
        let body = signed.post_body(&self.creds.api_key, PolyOrderType::FAK.as_str());

        // Post order
        let resp = self.inner.post_order_async(body, &self.creds).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Polymarket order failed {}: {}", status, body));
        }

        let resp_json: serde_json::Value = resp.json().await?;
        let order_id = resp_json["orderID"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        // Query fill status. The order endpoint can return before the order is visible to /data/order.
        // Retry briefly if we see a transient `null` response.
        let mut last_err: Option<anyhow::Error> = None;
        let order_info = 'retry: loop {
            // Up to ~1.2s total wait (including jitterless backoff).
            for attempt in 0..6 {
                match self.inner.get_order_async(&order_id, &self.creds).await {
                    Ok(info) => break 'retry Ok(info),
                    Err(e) => {
                        let msg = e.to_string();
                        last_err = Some(e);
                        if msg.contains("order not ready yet") && attempt < 5 {
                            sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
                            continue;
                        }
                        break 'retry Err(anyhow!("{}", msg));
                    }
                }
            }
            // If we get here without returning, surface the last error.
            break Err(last_err.unwrap_or_else(|| anyhow!("get_order failed (unknown error)")));
        }?;
        let filled_size: f64 = order_info.size_matched.parse().unwrap_or(0.0);
        let order_price: f64 = order_info.price.parse().unwrap_or(price);

        tracing::debug!(
            "[POLY-ASYNC] FAK {} {}: status={}, filled={:.2}/{:.2}, price={:.4}",
            side,
            order_id,
            order_info.status,
            filled_size,
            size,
            order_price
        );

        let is_delayed = order_info.status.eq_ignore_ascii_case("delayed");

        Ok(PolyFillAsync {
            order_id,
            filled_size,
            fill_cost: filled_size * order_price,
            is_delayed,
        })
    }

    /// Build a signed order.
    fn build_signed_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: &str,
        neg_risk: bool,
    ) -> Result<SignedOrder> {
        let price_bps = price_to_bps(price);
        let size_micro = size_to_micro(size);

        if !price_valid(price_bps) {
            return Err(anyhow!(
                "price {} ({}bps) outside allowed range",
                price,
                price_bps
            ));
        }

        let (side_code, maker_amt, taker_amt) = if side.eq_ignore_ascii_case("BUY") {
            get_order_amounts_buy(size_micro, price_bps)
        } else if side.eq_ignore_ascii_case("SELL") {
            get_order_amounts_sell(size_micro, price_bps)
        } else {
            return Err(anyhow!("side must be BUY or SELL"));
        };

        let salt = generate_seed();
        let maker_amount_str = maker_amt.to_string();
        let taker_amount_str = taker_amt.to_string();

        // Use references for EIP712 signing
        let data = OrderData {
            maker: &self.inner.funder,
            taker: ZERO_ADDRESS,
            token_id,
            maker_amount: &maker_amount_str,
            taker_amount: &taker_amount_str,
            side: side_code,
            fee_rate_bps: "0",
            nonce: "0",
            signer: &self.inner.wallet_address_str,
            expiration: "0",
            signature_type: self.inner.signature_type,
            salt,
        };
        let exchange = get_exchange_address(self.chain_id, neg_risk)?;
        let typed = order_typed_data(self.chain_id, &exchange, &data)?;
        let digest = typed.encode_eip712()?;

        let sig = self.inner.wallet.sign_hash(H256::from(digest))?;

        // Only allocate strings once for the final OrderStruct (serialization needs owned)
        Ok(SignedOrder {
            order: OrderStruct {
                salt,
                maker: self.inner.funder.clone(),
                signer: self.inner.wallet_address_str.clone(),
                taker: ZERO_ADDRESS.to_string(),
                token_id: token_id.to_string(),
                maker_amount: maker_amount_str,
                taker_amount: taker_amount_str,
                expiration: "0".to_string(),
                nonce: "0".to_string(),
                fee_rate_bps: "0".to_string(),
                side: side_code,
                signature_type: self.inner.signature_type,
            },
            signature: format!("0x{}", sig),
        })
    }

    /// Get reference to the inner client.
    #[allow(dead_code)]
    pub fn inner(&self) -> &PolymarketAsyncClient {
        &self.inner
    }

    /// Get reference to prepared credentials.
    #[allow(dead_code)]
    pub fn creds(&self) -> &PreparedCreds {
        &self.creds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_polymarket_clob_host_constant() {
        assert!(POLYMARKET_CLOB_HOST.contains("polymarket"));
        assert!(POLYMARKET_CLOB_HOST.starts_with("https://"));
    }

    #[test]
    fn test_host_trailing_slash_trimmed() {
        // Verify trailing slash handling
        let url_with_slash = "https://clob.polymarket.com/";
        let trimmed = url_with_slash.trim_end_matches('/');
        assert_eq!(trimmed, "https://clob.polymarket.com");
    }

    #[test]
    fn test_signed_order_post_body_format() {
        let signed = SignedOrder {
            order: OrderStruct {
                salt: 12345,
                maker: "0xmaker".to_string(),
                signer: "0xsigner".to_string(),
                taker: ZERO_ADDRESS.to_string(),
                token_id: "token123".to_string(),
                maker_amount: "1000000".to_string(),
                taker_amount: "500000".to_string(),
                expiration: "0".to_string(),
                nonce: "0".to_string(),
                fee_rate_bps: "0".to_string(),
                side: 0, // BUY
                signature_type: 1,
            },
            signature: "0xsig".to_string(),
        };

        let body = signed.post_body("owner_key", "FAK");

        // Verify it's valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("should be valid JSON");

        assert_eq!(parsed["owner"], "owner_key");
        assert_eq!(parsed["orderType"], "FAK");
        assert_eq!(parsed["order"]["maker"], "0xmaker");
        assert_eq!(parsed["order"]["tokenId"], "token123");
        assert_eq!(parsed["order"]["side"], "BUY");
    }

    #[test]
    fn test_signed_order_sell_side() {
        let signed = SignedOrder {
            order: OrderStruct {
                salt: 12345,
                maker: "0xmaker".to_string(),
                signer: "0xsigner".to_string(),
                taker: ZERO_ADDRESS.to_string(),
                token_id: "token123".to_string(),
                maker_amount: "1000000".to_string(),
                taker_amount: "500000".to_string(),
                expiration: "0".to_string(),
                nonce: "0".to_string(),
                fee_rate_bps: "0".to_string(),
                side: 1, // SELL
                signature_type: 1,
            },
            signature: "0xsig".to_string(),
        };

        let body = signed.post_body("owner_key", "FAK");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("should be valid JSON");
        assert_eq!(parsed["order"]["side"], "SELL");
    }
}
