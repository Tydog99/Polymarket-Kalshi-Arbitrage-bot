//! Polymarket CLOB (Central Limit Order Book) order execution client.
//!
//! This module provides high-performance order execution for the Polymarket CLOB,
//! including pre-computed authentication credentials and optimized request handling.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use async_trait::async_trait;

use crate::poly_executor::PolyExecutor;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::H256;
use ethers::types::transaction::eip712::{Eip712, TypedData};
use ethers::types::U256;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;

const USER_AGENT: &str = "py_clob_client";
const MSG_TO_SIGN: &str = "This message attests that I control the given wallet";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

// ============================================================================
// PRE-COMPUTED EIP712 CONSTANTS
// ============================================================================

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCreds {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(rename = "secret")]
    pub api_secret: String,
    #[serde(rename = "passphrase")]
    pub api_passphrase: String,
}

// ============================================================================
// PREPARED CREDENTIALS
// ============================================================================

#[derive(Clone)]
pub struct PreparedCreds {
    pub api_key: String,
    hmac_template: HmacSha256,
    api_key_header: HeaderValue,
    passphrase_header: HeaderValue,
}

impl PreparedCreds {
    pub fn from_api_creds(creds: &ApiCreds) -> Result<Self> {
        let decoded_secret = URL_SAFE.decode(&creds.api_secret)?;
        let hmac_template = HmacSha256::new_from_slice(&decoded_secret)
            .map_err(|e| anyhow!("Invalid HMAC key: {}", e))?;

        let api_key_header = HeaderValue::from_str(&creds.api_key)
            .map_err(|e| anyhow!("Invalid API key for header: {}", e))?;
        let passphrase_header = HeaderValue::from_str(&creds.api_passphrase)
            .map_err(|e| anyhow!("Invalid passphrase for header: {}", e))?;

        Ok(Self {
            api_key: creds.api_key.clone(),
            hmac_template,
            api_key_header,
            passphrase_header,
        })
    }

    /// Sign message using prewarmed HMAC
    #[inline]
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        let mut mac = self.hmac_template.clone();
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    }

    /// Sign and return base64 (for L2 headers)
    #[inline]
    pub fn sign_b64(&self, message: &[u8]) -> String {
        URL_SAFE.encode(self.sign(message))
    }

    /// Get cached API key header
    #[inline]
    pub fn api_key_header(&self) -> HeaderValue {
        self.api_key_header.clone()
    }

    /// Get cached passphrase header
    #[inline]
    pub fn passphrase_header(&self) -> HeaderValue {
        self.passphrase_header.clone()
    }
}

fn add_default_headers(headers: &mut HeaderMap) {
    headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
}

#[inline(always)]
fn current_unix_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn clob_auth_digest(chain_id: u64, address_str: &str, timestamp: u64, nonce: u64) -> Result<H256> {
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

/// Order data for EIP712 signing (references to avoid clones in hot path)
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
    salt: u128
}

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

#[derive(Debug, Clone, Serialize)]
pub struct SignedOrder { 
    pub order: OrderStruct, 
    pub signature: String 
}

impl SignedOrder {
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

#[inline(always)]
fn generate_seed() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() % u128::from(u32::MAX)
}

// ============================================================================
// ORDER CALCULATIONS
// ============================================================================

/// Convert f64 price (0.0-1.0) to basis points (0-10000)
/// e.g., 0.65 -> 6500
#[inline(always)]
pub fn price_to_bps(price: f64) -> u64 {
    ((price * 10000.0).round() as i64).max(0) as u64
}

/// Convert f64 size to micro-units (6 decimal places)
/// e.g., 100.5 -> 100_500_000
#[inline(always)]
pub fn size_to_micro(size: f64) -> u64 {
    ((size * 1_000_000.0).floor() as i64).max(0) as u64
}

/// BUY order calculation
/// Input: size in micro-units, price in basis points
/// Output: (side=0, maker_amount, taker_amount) in token decimals (6 dp)
#[inline(always)]
pub fn get_order_amounts_buy(size_micro: u64, price_bps: u64) -> (i32, u128, u128) {
    // For BUY: taker = size (what we receive), maker = size * price (what we pay)
    let taker = size_micro as u128;
    // maker = size * price / 10000 (convert bps to ratio)
    let maker = (size_micro as u128 * price_bps as u128) / 10000;
    (0, maker, taker)
}

/// SELL order calculation
/// Input: size in micro-units, price in basis points
/// Output: (side=1, maker_amount, taker_amount) in token decimals (6 dp)
#[inline(always)]
pub fn get_order_amounts_sell(size_micro: u64, price_bps: u64) -> (i32, u128, u128) {
    // For SELL: maker = size (what we give), taker = size * price (what we receive)
    let maker = size_micro as u128;
    // taker = size * price / 10000 (convert bps to ratio)
    let taker = (size_micro as u128 * price_bps as u128) / 10000;
    (1, maker, taker)
}

/// Validate price is within allowed range for tick=0.01
#[inline(always)]
pub fn price_valid(price_bps: u64) -> bool {
    // For tick=0.01: price must be >= 0.01 (100 bps) and <= 0.99 (9900 bps)
    (100..=9900).contains(&price_bps)
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

fn get_exchange_address(chain_id: u64, neg_risk: bool) -> Result<String> {
    match (chain_id, neg_risk) {
        (137, true) => Ok("0xC5d563A36AE78145C45a50134d48A1215220f80a".into()),
        (137, false) => Ok("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".into()),
        (80002, true) => Ok("0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296".into()),
        (80002, false) => Ok("0xdFE02Eb6733538f8Ea35D585af8DE5958AD99E40".into()),
        _ => Err(anyhow!("unsupported chain")),
    }
}

// ============================================================================
// ORDER TYPES FOR FAK/FOK
// ============================================================================

/// Order type for Polymarket
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
#[allow(clippy::upper_case_acronyms)]
pub enum PolyOrderType {
    /// Good Till Cancelled (default)
    GTC,
    /// Good Till Time
    GTD,
    /// Fill Or Kill - must fill entirely or cancel
    FOK,
    /// Fill And Kill - fill what you can, cancel rest
    FAK,
}

impl PolyOrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolyOrderType::GTC => "GTC",
            PolyOrderType::GTD => "GTD",
            PolyOrderType::FOK => "FOK",
            PolyOrderType::FAK => "FAK",
        }
    }
}

// ============================================================================
// GET ORDER RESPONSE
// ============================================================================

/// Response from GET /data/order/{order_id}
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PolymarketOrderResponse {
    pub id: String,
    pub status: String,
    pub market: Option<String>,
    pub outcome: Option<String>,
    pub price: String,
    pub side: String,
    pub size_matched: String,
    pub original_size: String,
    pub maker_address: Option<String>,
    pub asset_id: Option<String>,
    #[serde(default)]
    pub associate_trades: Vec<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<serde_json::Value>,  // Can be string or integer
    #[serde(default)]
    pub expiration: Option<serde_json::Value>,  // Can be string or integer
    #[serde(rename = "type")]
    pub order_type: Option<String>,
    pub owner: Option<String>,
}

// ============================================================================
// BALANCE RESPONSE
// ============================================================================

/// Response from GET /balance-allowance?asset_type=COLLATERAL
///
/// Balance and allowances are returned as strings in 6-decimal USDC format.
/// For example, "1500000000" represents 1500 USDC ($1500.00 = 150000 cents).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolyBalance {
    /// USDC.e balance in 6 decimals (string)
    pub balance: String,
    /// Approved allowances per contract address in 6 decimals (map of address -> string)
    pub allowances: std::collections::HashMap<String, String>,
}

impl PolyBalance {
    /// Convert balance string to cents (divide by 10000).
    ///
    /// USDC has 6 decimals, so 1000000 = 1 USDC = 100 cents.
    /// To convert to cents: value / 10000
    pub fn balance_as_cents(&self) -> i64 {
        self.balance.parse::<i64>().unwrap_or(0) / 10000
    }

    /// Convert allowance string to cents (divide by 10000).
    /// Returns the maximum allowance across all contracts.
    pub fn allowance_as_cents(&self) -> i64 {
        self.allowances
            .values()
            .filter_map(|v| v.parse::<i64>().ok())
            .max()
            .unwrap_or(0)
            / 10000
    }
}

// ============================================================================
// ASYNC CLIENT
// ============================================================================

/// Async Polymarket client for execution
pub struct PolymarketAsyncClient {
    host: String,
    chain_id: u64,
    http: reqwest_middleware::ClientWithMiddleware,  // Async client with connection pooling + capture
    wallet: Arc<LocalWallet>,
    funder: String,
    wallet_address_str: String,
    address_header: HeaderValue,
    /// Signature type for order signing (0=EOA, 1=poly proxy, 2=gnosis safe)
    signature_type: i32,
}

impl PolymarketAsyncClient {
    pub fn new(host: &str, chain_id: u64, private_key: &str, funder: &str, signature_type: i32) -> Result<Self> {
        let wallet = private_key.parse::<LocalWallet>()?.with_chain_id(chain_id);
        let wallet_address_str = format!("{:?}", wallet.address());
        let address_header = HeaderValue::from_str(&wallet_address_str)
            .map_err(|e| anyhow!("Invalid wallet address for header: {}", e))?;

        // Build async client with connection pooling, keepalive, and HTTP capture
        let http = trading::capture::build_poly_client(std::time::Duration::from_secs(10));

        Ok(Self {
            host: host.trim_end_matches('/').to_string(),
            chain_id,
            http,
            wallet: Arc::new(wallet),
            funder: funder.to_string(),
            wallet_address_str,
            address_header,
            signature_type,
        })
    }

    /// Build L1 headers for authentication (derive-api-key)
    /// wallet.sign_hash() is CPU-bound (~1ms), safe to call in async context
    fn build_l1_headers(&self, nonce: u64) -> Result<HeaderMap> {
        let timestamp = current_unix_ts();
        let digest = clob_auth_digest(self.chain_id, &self.wallet_address_str, timestamp, nonce)?;
        let sig = self.wallet.sign_hash(digest)?;
        let mut headers = HeaderMap::new();
        headers.insert("POLY_ADDRESS", self.address_header.clone());
        headers.insert("POLY_SIGNATURE", HeaderValue::from_str(&format!("0x{}", sig))?);
        headers.insert("POLY_TIMESTAMP", HeaderValue::from_str(&timestamp.to_string())?);
        headers.insert("POLY_NONCE", HeaderValue::from_str(&nonce.to_string())?);
        add_default_headers(&mut headers);
        Ok(headers)
    }

    /// Derive API credentials from L1 wallet signature
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

    /// Build L2 headers for authenticated requests
    fn build_l2_headers(&self, method: &str, path: &str, body: Option<&str>, creds: &PreparedCreds) -> Result<HeaderMap> {
        let timestamp = current_unix_ts();
        let mut message = format!("{}{}{}", timestamp, method, path);
        if let Some(b) = body { message.push_str(b); }

        let sig_b64 = creds.sign_b64(message.as_bytes());

        let mut headers = HeaderMap::with_capacity(9);
        headers.insert("POLY_ADDRESS", self.address_header.clone());
        headers.insert("POLY_SIGNATURE", HeaderValue::from_str(&sig_b64)?);
        headers.insert("POLY_TIMESTAMP", HeaderValue::from_str(&timestamp.to_string())?);
        headers.insert("POLY_API_KEY", creds.api_key_header());
        headers.insert("POLY_PASSPHRASE", creds.passphrase_header());
        add_default_headers(&mut headers);
        Ok(headers)
    }

    /// Post order 
    pub async fn post_order_async(&self, body: String, creds: &PreparedCreds) -> Result<reqwest::Response> {
        let path = "/order";
        let url = format!("{}{}", self.host, path);
        let headers = self.build_l2_headers("POST", path, Some(&body), creds)?;

        let resp = self.http
            .post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        Ok(resp)
    }

    /// Get order by ID
    pub async fn get_order_async(&self, order_id: &str, creds: &PreparedCreds) -> Result<PolymarketOrderResponse> {
        let path = format!("/data/order/{}", order_id);
        let url = format!("{}{}", self.host, path);
        let headers = self.build_l2_headers("GET", &path, None, creds)?;

        let resp = self.http
            .get(&url)
            .headers(headers)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(anyhow!("get_order failed {}: {}", status, body));
        }

        // Log raw response for debugging order status issues
        tracing::debug!("[POLY] get_order {} response: {}", order_id, body);

        // Handle null response (order doesn't exist or was rejected)
        if body.trim() == "null" || body.trim().is_empty() {
            return Err(anyhow!("get_order {} returned null - order may have been rejected", order_id));
        }

        serde_json::from_str(&body)
            .map_err(|e| anyhow!("get_order {} parse error: {} (body: {})", order_id, e, body))
    }

    /// Get balance and allowance for the wallet.
    ///
    /// Makes authenticated GET request to /balance-allowance?asset_type=COLLATERAL&signature_type=N
    /// The signature_type parameter is required for proxy wallets (1=poly proxy, 2=gnosis safe)
    /// to return the funder's balance instead of the signer's balance.
    pub async fn get_balance_async(&self, creds: &PreparedCreds) -> Result<PolyBalance> {
        // Signature is computed over base path only (per Polymarket py-clob-client)
        let sign_path = "/balance-allowance";
        let url = format!(
            "{}/balance-allowance?asset_type=COLLATERAL&signature_type={}",
            self.host, self.signature_type
        );
        let headers = self.build_l2_headers("GET", sign_path, None, creds)?;

        let resp = self.http
            .get(&url)
            .headers(headers)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(anyhow!("get_balance failed {}: {}", status, body));
        }

        serde_json::from_str(&body)
            .map_err(|e| anyhow!("get_balance parse error: {} (body: {})", e, body))
    }

    /// Check neg_risk for token - with caching
    /// Returns an error if the API response is malformed (missing neg_risk field or wrong type)
    pub async fn check_neg_risk(&self, token_id: &str) -> Result<bool> {
        let url = format!("{}/neg-risk?token_id={}", self.host, token_id);
        let resp = self.http
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("neg-risk API returned {}: {}", status, body));
        }

        let val: serde_json::Value = resp.json().await?;
        match val.get("neg_risk") {
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(other) => Err(anyhow!(
                "neg-risk API returned non-boolean neg_risk for token {}: {:?}",
                token_id,
                other
            )),
            None => Err(anyhow!(
                "neg-risk API response missing neg_risk field for token {}: {}",
                token_id,
                val
            )),
        }
    }

    #[allow(dead_code)]
    pub fn wallet_address(&self) -> &str {
        &self.wallet_address_str
    }

    #[allow(dead_code)]
    pub fn funder(&self) -> &str {
        &self.funder
    }

    #[allow(dead_code)]
    pub fn wallet(&self) -> &LocalWallet {
        &self.wallet
    }
}

/// Shared async client wrapper for use in execution engine
pub struct SharedAsyncClient {
    inner: Arc<PolymarketAsyncClient>,
    creds: PreparedCreds,
    chain_id: u64,
    /// Pre-cached neg_risk lookups
    neg_risk_cache: std::sync::RwLock<HashMap<String, bool>>,
}

impl SharedAsyncClient {
    pub fn new(client: PolymarketAsyncClient, creds: PreparedCreds, chain_id: u64) -> Self {
        Self {
            inner: Arc::new(client),
            creds,
            chain_id,
            neg_risk_cache: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Load neg_risk cache from JSON file (output of build_sports_cache.py)
    pub fn load_cache(&self, path: &str) -> Result<usize> {
        let data = std::fs::read_to_string(path)?;
        let map: HashMap<String, bool> = serde_json::from_str(&data)?;
        let count = map.len();
        let mut cache = self.neg_risk_cache.write().unwrap();
        *cache = map;
        Ok(count)
    }

    /// Populate neg_risk cache directly from MarketPairs discovered at runtime.
    /// This eliminates the need for the separate Python cache-warming script.
    pub fn populate_cache_from_pairs(&self, pairs: &[crate::types::MarketPair]) -> usize {
        let mut cache = self.neg_risk_cache.write().unwrap();
        let mut count = 0;
        for pair in pairs {
            // Insert both YES and NO tokens with the same neg_risk value
            cache.insert(pair.poly_yes_token.to_string(), pair.neg_risk);
            cache.insert(pair.poly_no_token.to_string(), pair.neg_risk);
            count += 2;
        }
        count
    }

    /// Get balance and allowance for the wallet.
    ///
    /// Returns USDC.e balance and approved allowance.
    /// Use `balance.balance_as_cents()` to convert to cents.
    pub async fn get_balance(&self) -> Result<PolyBalance> {
        self.inner.get_balance_async(&self.creds).await
    }

    /// Execute FAK buy order - 
    pub async fn buy_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
        debug_assert!(!token_id.is_empty(), "token_id must not be empty");
        debug_assert!(price > 0.0 && price < 1.0, "price must be 0 < p < 1");
        debug_assert!(size >= 1.0, "size must be >= 1");
        self.execute_order(token_id, price, size, "BUY").await
    }

    /// Execute FAK sell order -
    pub async fn sell_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
        debug_assert!(!token_id.is_empty(), "token_id must not be empty");
        debug_assert!(price > 0.0 && price < 1.0, "price must be 0 < p < 1");
        debug_assert!(size >= 1.0, "size must be >= 1");
        self.execute_order(token_id, price, size, "SELL").await
    }

    /// Poll a delayed order until it reaches a terminal state or times out.
    ///
    /// Returns (size_matched, fill_cost) where fill_cost = size_matched * price.
    /// For canceled/expired orders, returns (0.0, 0.0).
    pub async fn poll_delayed_order(
        &self,
        order_id: &str,
        price: f64,
        timeout_ms: u64,
    ) -> Result<(f64, f64)> {
        const BACKOFF_MS: [u64; 6] = [100, 200, 500, 1000, 1500, 2000];

        let start = Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        let mut attempt = 0;

        loop {
            // Check timeout before polling
            if start.elapsed() >= timeout {
                return Err(anyhow!(
                    "Order {} still pending after {}ms",
                    order_id,
                    timeout_ms
                ));
            }

            // Poll the order
            match self.inner.get_order_async(order_id, &self.creds).await {
                Ok(order) => {
                    let status = order.status.to_lowercase();

                    match status.as_str() {
                        "matched" | "filled" => {
                            // Terminal success - parse size_matched and compute fill cost
                            let size_matched: f64 = order.size_matched.parse().unwrap_or(0.0);
                            let fill_cost = size_matched * price;
                            tracing::info!(
                                "[POLY] Order {} reached terminal state '{}': matched={:.2}, cost=${:.2}",
                                order_id, status, size_matched, fill_cost
                            );
                            return Ok((size_matched, fill_cost));
                        }
                        "canceled" | "expired" => {
                            // Terminal failure - no fill
                            tracing::info!(
                                "[POLY] Order {} reached terminal state '{}': no fill",
                                order_id, status
                            );
                            return Ok((0.0, 0.0));
                        }
                        "delayed" | "live" => {
                            // Non-terminal - continue polling
                            tracing::debug!(
                                "[POLY] Order {} still in '{}' state, attempt {}",
                                order_id, status, attempt + 1
                            );
                        }
                        _ => {
                            // Unknown status - treat as non-terminal and continue
                            tracing::warn!(
                                "[POLY] Order {} has unknown status '{}', continuing to poll",
                                order_id, status
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[POLY] Error polling order {}: {}, continuing",
                        order_id, e
                    );
                }
            }

            // Calculate backoff delay (use last value if beyond array)
            let delay_ms = BACKOFF_MS.get(attempt).copied().unwrap_or(BACKOFF_MS[BACKOFF_MS.len() - 1]);

            // Don't sleep if we would exceed timeout
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(anyhow!(
                    "Order {} still pending after {}ms",
                    order_id,
                    timeout_ms
                ));
            }

            let sleep_duration = Duration::from_millis(delay_ms).min(remaining);
            tokio::time::sleep(sleep_duration).await;

            attempt += 1;
        }
    }

    async fn execute_order(&self, token_id: &str, price: f64, size: f64, side: &str) -> Result<PolyFillAsync> {
        // Check neg_risk cache first
        let neg_risk = {
            let cache = self.neg_risk_cache.read().unwrap();
            cache.get(token_id).copied()
        };

        let neg_risk = match neg_risk {
            Some(nr) => nr,
            None => {
                // WARNING: neg_risk not in cache - this means discovery didn't populate it
                // This triggers an extra API call and may indicate a bug in discovery
                tracing::warn!(
                    "⚠️ [POLY] neg_risk CACHE MISS for token {}... - fetching from API (discovery may have failed to populate)",
                    &token_id[..token_id.len().min(20)]
                );
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
        tracing::info!(
            "[POLY] Posting {} order: token={}, price={:.4}, size={:.2}, neg_risk={}",
            side, token_id, price, size, neg_risk
        );

        let resp = self.inner.post_order_async(body, &self.creds).await?;
        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();

        tracing::debug!("[POLY] POST /order response {}: {}", status, resp_body);

        if !status.is_success() {
            return Err(anyhow!("Polymarket order failed {}: {}", status, resp_body));
        }

        let resp_json: serde_json::Value = serde_json::from_str(&resp_body)
            .map_err(|e| anyhow!("Failed to parse order response: {} (body: {})", e, resp_body))?;
        let order_id = resp_json["orderID"].as_str().unwrap_or("unknown").to_string();
        let order_status = resp_json["status"].as_str().unwrap_or("unknown");
        let is_delayed = order_status == "delayed";

        // Extract fill info directly from POST response (no need to call get_order)
        // For BUY: takingAmount = shares received, makingAmount = USDC spent
        // For SELL: takingAmount = USDC received, makingAmount = shares sold
        // Note: When status="delayed", amounts are empty strings - this is expected
        let (filled_size, fill_cost) = if side.eq_ignore_ascii_case("BUY") {
            let shares: f64 = match resp_json["takingAmount"].as_str() {
                Some(s) if !s.is_empty() => s.parse().unwrap_or_else(|e| {
                    tracing::warn!("[POLY] Failed to parse takingAmount '{}': {}", s, e);
                    0.0
                }),
                Some(_) if is_delayed => 0.0, // Empty string expected for delayed orders
                Some(s) => {
                    tracing::warn!("[POLY] Empty takingAmount in non-delayed response: {}", resp_json);
                    s.parse().unwrap_or(0.0)
                }
                None if is_delayed => 0.0, // Missing expected for delayed orders
                None => {
                    tracing::warn!("[POLY] takingAmount not present in response: {}", resp_json);
                    0.0
                }
            };
            let cost: f64 = match resp_json["makingAmount"].as_str() {
                Some(s) if !s.is_empty() => s.parse().unwrap_or_else(|e| {
                    tracing::warn!("[POLY] Failed to parse makingAmount '{}': {}", s, e);
                    0.0
                }),
                Some(_) if is_delayed => 0.0, // Empty string expected for delayed orders
                Some(s) => {
                    tracing::warn!("[POLY] Empty makingAmount in non-delayed response: {}", resp_json);
                    s.parse().unwrap_or(0.0)
                }
                None if is_delayed => 0.0, // Missing expected for delayed orders
                None => {
                    tracing::warn!("[POLY] makingAmount not present in response: {}", resp_json);
                    0.0
                }
            };
            (shares, cost)
        } else {
            // SELL: makingAmount is shares sold, takingAmount is USDC received
            let shares: f64 = match resp_json["makingAmount"].as_str() {
                Some(s) if !s.is_empty() => s.parse().unwrap_or_else(|e| {
                    tracing::warn!("[POLY] Failed to parse makingAmount '{}': {}", s, e);
                    0.0
                }),
                Some(_) if is_delayed => 0.0, // Empty string expected for delayed orders
                Some(s) => {
                    tracing::warn!("[POLY] Empty makingAmount in non-delayed response: {}", resp_json);
                    s.parse().unwrap_or(0.0)
                }
                None if is_delayed => 0.0, // Missing expected for delayed orders
                None => {
                    tracing::warn!("[POLY] makingAmount not present in response: {}", resp_json);
                    0.0
                }
            };
            let cost: f64 = match resp_json["takingAmount"].as_str() {
                Some(s) if !s.is_empty() => s.parse().unwrap_or_else(|e| {
                    tracing::warn!("[POLY] Failed to parse takingAmount '{}': {}", s, e);
                    0.0
                }),
                Some(_) if is_delayed => 0.0, // Empty string expected for delayed orders
                Some(s) => {
                    tracing::warn!("[POLY] Empty takingAmount in non-delayed response: {}", resp_json);
                    s.parse().unwrap_or(0.0)
                }
                None if is_delayed => 0.0, // Missing expected for delayed orders
                None => {
                    tracing::warn!("[POLY] takingAmount not present in response: {}", resp_json);
                    0.0
                }
            };
            (shares, cost)
        };

        tracing::info!(
            "[POLY] FAK {} {}: status={}, filled={:.2}/{:.2}, cost=${:.2}{}",
            side, order_id, order_status, filled_size, size, fill_cost,
            if is_delayed { " (DELAYED)" } else { "" }
        );

        Ok(PolyFillAsync {
            order_id,
            filled_size,
            fill_cost,
            is_delayed,
        })
    }

    /// Build a signed order
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
            return Err(anyhow!("price {} ({}bps) outside allowed range", price, price_bps));
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
}

/// Async fill result
#[derive(Debug, Clone)]
pub struct PolyFillAsync {
    pub order_id: String,
    pub filled_size: f64,
    pub fill_cost: f64,
    /// True when Polymarket returns status="delayed" - fill may come later
    pub is_delayed: bool,
}

// =============================================================================
// POLY EXECUTOR TRAIT IMPLEMENTATION
// =============================================================================

#[async_trait]
impl PolyExecutor for SharedAsyncClient {
    async fn buy_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
        SharedAsyncClient::buy_fak(self, token_id, price, size).await
    }

    async fn sell_fak(&self, token_id: &str, price: f64, size: f64) -> Result<PolyFillAsync> {
        SharedAsyncClient::sell_fak(self, token_id, price, size).await
    }

    async fn poll_delayed_order(
        &self,
        order_id: &str,
        price: f64,
        timeout_ms: u64,
    ) -> Result<(f64, f64)> {
        SharedAsyncClient::poll_delayed_order(self, order_id, price, timeout_ms).await
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Test private key (DO NOT use in production - this is a well-known test key)
    const TEST_PRIVATE_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_FUNDER: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    #[test]
    fn test_client_stores_signature_type_eoa() {
        let client = PolymarketAsyncClient::new(
            "https://clob.polymarket.com",
            137,
            TEST_PRIVATE_KEY,
            TEST_FUNDER,
            0, // EOA
        ).expect("should create client");

        assert_eq!(client.signature_type, 0);
    }

    #[test]
    fn test_client_stores_signature_type_poly_proxy() {
        let client = PolymarketAsyncClient::new(
            "https://clob.polymarket.com",
            137,
            TEST_PRIVATE_KEY,
            TEST_FUNDER,
            1, // Poly proxy
        ).expect("should create client");

        assert_eq!(client.signature_type, 1);
    }

    #[test]
    fn test_client_stores_signature_type_gnosis_safe() {
        let client = PolymarketAsyncClient::new(
            "https://clob.polymarket.com",
            137,
            TEST_PRIVATE_KEY,
            TEST_FUNDER,
            2, // Gnosis safe
        ).expect("should create client");

        assert_eq!(client.signature_type, 2);
    }

    #[test]
    fn test_parse_order_response_valid() {
        let json = r#"{
            "id": "0x123",
            "status": "MATCHED",
            "market": "0xabc",
            "outcome": "Yes",
            "price": "0.65",
            "side": "BUY",
            "size_matched": "10",
            "original_size": "10",
            "maker_address": "0xdef",
            "asset_id": "12345"
        }"#;

        let result: Result<PolymarketOrderResponse, _> = serde_json::from_str(json);
        assert!(result.is_ok());
        let order = result.unwrap();
        assert_eq!(order.id, "0x123");
        assert_eq!(order.status, "MATCHED");
        assert_eq!(order.size_matched, "10");
    }

    #[test]
    fn test_parse_order_response_null_fails() {
        let json = "null";
        let result: Result<PolymarketOrderResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("null"), "Error should mention null: {}", err);
    }

    #[test]
    fn test_parse_order_response_empty_fails() {
        let json = "";
        let result: Result<PolymarketOrderResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_null_response_detection() {
        // Test the exact null check logic used in get_order_async
        let null_responses = ["null", " null ", "null\n", "\nnull\n"];
        for response in null_responses {
            assert!(
                response.trim() == "null" || response.trim().is_empty(),
                "Should detect '{}' as null response",
                response
            );
        }
    }

    #[test]
    fn test_valid_response_not_detected_as_null() {
        let valid_responses = [
            r#"{"id":"123"}"#,
            r#"{"id":"123","status":"MATCHED"}"#,
        ];
        for response in valid_responses {
            assert!(
                response.trim() != "null" && !response.trim().is_empty(),
                "Should NOT detect '{}' as null response",
                response
            );
        }
    }

    #[test]
    fn test_post_response_fill_extraction_buy() {
        // Real POST response from Polymarket for a BUY order
        let json = r#"{
            "errorMsg": "",
            "orderID": "0x8b416d01048aea99086ea6c1e4510cad2d0bb881a0def312b9b81b7ac732ad8e",
            "takingAmount": "5",
            "makingAmount": "2.35",
            "status": "matched",
            "transactionsHashes": ["0xfa6dc3d89588e8024a8c729a3380f7a4ec1be0ba6788894fd8c9a2fb5329b706"],
            "success": true
        }"#;

        let resp_json: serde_json::Value = serde_json::from_str(json).unwrap();

        // For BUY: takingAmount = shares received, makingAmount = USDC spent
        let shares: f64 = resp_json["takingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let cost: f64 = resp_json["makingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        assert_eq!(shares, 5.0, "Should extract 5 shares from takingAmount");
        assert_eq!(cost, 2.35, "Should extract $2.35 cost from makingAmount");
    }

    #[test]
    fn test_post_response_fill_extraction_sell() {
        // POST response for a SELL order (hypothetical - amounts are reversed)
        let json = r#"{
            "errorMsg": "",
            "orderID": "0xabc123",
            "takingAmount": "3.50",
            "makingAmount": "5",
            "status": "matched",
            "transactionsHashes": [],
            "success": true
        }"#;

        let resp_json: serde_json::Value = serde_json::from_str(json).unwrap();

        // For SELL: makingAmount = shares sold, takingAmount = USDC received
        let shares: f64 = resp_json["makingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let cost: f64 = resp_json["takingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        assert_eq!(shares, 5.0, "Should extract 5 shares from makingAmount");
        assert_eq!(cost, 3.50, "Should extract $3.50 from takingAmount");
    }

    #[test]
    fn test_post_response_fill_extraction_no_fill() {
        // POST response when order doesn't fill (FAK with no match)
        let json = r#"{
            "errorMsg": "",
            "orderID": "0xdef456",
            "status": "LIVE",
            "success": true
        }"#;

        let resp_json: serde_json::Value = serde_json::from_str(json).unwrap();

        // Missing takingAmount/makingAmount should default to 0.0
        let shares: f64 = resp_json["takingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let cost: f64 = resp_json["makingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        assert_eq!(shares, 0.0, "Missing takingAmount should default to 0");
        assert_eq!(cost, 0.0, "Missing makingAmount should default to 0");
    }

    #[test]
    fn test_post_response_fill_extraction_partial_fill() {
        // POST response for partial fill
        let json = r#"{
            "errorMsg": "",
            "orderID": "0x789ghi",
            "takingAmount": "3",
            "makingAmount": "1.47",
            "status": "matched",
            "success": true
        }"#;

        let resp_json: serde_json::Value = serde_json::from_str(json).unwrap();
        let order_status = resp_json["status"].as_str().unwrap_or("unknown");

        let shares: f64 = resp_json["takingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let cost: f64 = resp_json["makingAmount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        assert_eq!(order_status, "matched");
        assert_eq!(shares, 3.0, "Should extract partial fill of 3 shares");
        assert_eq!(cost, 1.47, "Should extract cost of $1.47");
    }

    // Helper to create a SharedAsyncClient for testing cache behavior
    fn create_test_shared_client() -> SharedAsyncClient {
        let client = PolymarketAsyncClient::new(
            "https://clob.polymarket.com",
            137,
            TEST_PRIVATE_KEY,
            TEST_FUNDER,
            0,
        ).expect("should create client");

        let api_creds = super::ApiCreds {
            api_key: "test_key".to_string(),
            api_secret: "dGVzdF9zZWNyZXQ=".to_string(), // base64 encoded "test_secret"
            api_passphrase: "test_pass".to_string(),
        };
        let creds = PreparedCreds::from_api_creds(&api_creds).expect("should create creds");

        SharedAsyncClient::new(client, creds, 137)
    }

    // Helper to create a MarketPair for testing
    fn create_test_market_pair(
        pair_id: &str,
        yes_token: &str,
        no_token: &str,
        neg_risk: bool,
    ) -> crate::types::MarketPair {
        crate::types::MarketPair {
            pair_id: pair_id.into(),
            league: "test".into(),
            market_type: crate::types::MarketType::Moneyline,
            description: "Test market".into(),
            kalshi_event_ticker: "TEST-EVENT".into(),
            kalshi_market_ticker: "TEST-MARKET".into(),
            kalshi_event_slug: "test-event".into(),
            poly_slug: "test-slug".into(),
            poly_yes_token: yes_token.into(),
            poly_no_token: no_token.into(),
            line_value: None,
            team_suffix: None,
            neg_risk,
        }
    }

    #[test]
    fn test_populate_cache_from_pairs_empty() {
        let client = create_test_shared_client();
        let pairs: Vec<crate::types::MarketPair> = vec![];

        let count = client.populate_cache_from_pairs(&pairs);

        assert_eq!(count, 0, "Empty pairs should return 0");
        let cache = client.neg_risk_cache.read().unwrap();
        assert!(cache.is_empty(), "Cache should be empty");
    }

    #[test]
    fn test_populate_cache_from_pairs_single() {
        let client = create_test_shared_client();
        let pairs = vec![
            create_test_market_pair("pair1", "yes_token_1", "no_token_1", false),
        ];

        let count = client.populate_cache_from_pairs(&pairs);

        assert_eq!(count, 2, "Single pair should add 2 entries (YES + NO)");

        let cache = client.neg_risk_cache.read().unwrap();
        assert_eq!(cache.get("yes_token_1"), Some(&false));
        assert_eq!(cache.get("no_token_1"), Some(&false));
    }

    #[test]
    fn test_populate_cache_from_pairs_multiple_mixed() {
        let client = create_test_shared_client();
        let pairs = vec![
            create_test_market_pair("pair1", "yes_token_1", "no_token_1", false),
            create_test_market_pair("pair2", "yes_token_2", "no_token_2", true),
            create_test_market_pair("pair3", "yes_token_3", "no_token_3", false),
        ];

        let count = client.populate_cache_from_pairs(&pairs);

        assert_eq!(count, 6, "3 pairs should add 6 entries");

        let cache = client.neg_risk_cache.read().unwrap();
        // neg_risk=false pair
        assert_eq!(cache.get("yes_token_1"), Some(&false));
        assert_eq!(cache.get("no_token_1"), Some(&false));
        // neg_risk=true pair
        assert_eq!(cache.get("yes_token_2"), Some(&true));
        assert_eq!(cache.get("no_token_2"), Some(&true));
        // neg_risk=false pair
        assert_eq!(cache.get("yes_token_3"), Some(&false));
        assert_eq!(cache.get("no_token_3"), Some(&false));
    }

    #[test]
    fn test_populate_cache_overwrites_existing() {
        let client = create_test_shared_client();

        // Pre-populate cache with a different value
        {
            let mut cache = client.neg_risk_cache.write().unwrap();
            cache.insert("yes_token_1".to_string(), true); // Will be overwritten
            cache.insert("no_token_1".to_string(), true);  // Will be overwritten
        }

        // Now populate from pairs with neg_risk=false
        let pairs = vec![
            create_test_market_pair("pair1", "yes_token_1", "no_token_1", false),
        ];

        let count = client.populate_cache_from_pairs(&pairs);
        assert_eq!(count, 2);

        // Verify overwrite occurred
        let cache = client.neg_risk_cache.read().unwrap();
        assert_eq!(cache.get("yes_token_1"), Some(&false), "Should overwrite with discovery value");
        assert_eq!(cache.get("no_token_1"), Some(&false), "Should overwrite with discovery value");
    }

    #[test]
    fn test_populate_cache_neg_risk_true() {
        let client = create_test_shared_client();
        let pairs = vec![
            create_test_market_pair("pair_neg", "neg_yes", "neg_no", true),
        ];

        let count = client.populate_cache_from_pairs(&pairs);
        assert_eq!(count, 2);

        let cache = client.neg_risk_cache.read().unwrap();
        assert_eq!(cache.get("neg_yes"), Some(&true), "neg_risk=true should be stored");
        assert_eq!(cache.get("neg_no"), Some(&true), "neg_risk=true should be stored");
    }
}