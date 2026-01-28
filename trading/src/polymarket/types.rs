//! Polymarket order types and calculation utilities.
//!
//! This module provides:
//! - API credential structures for CLOB authentication
//! - Order type definitions (GTC, GTD, FOK, FAK)
//! - Price/size conversion functions for order placement
//! - Order response structures

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

// ============================================================================
// API CREDENTIALS
// ============================================================================

/// API credentials for Polymarket CLOB authentication.
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

/// Pre-computed credentials for fast HMAC signing.
///
/// This structure caches the HMAC template and header values to avoid
/// repeated allocations during order signing in the hot path.
#[derive(Clone)]
pub struct PreparedCreds {
    pub api_key: String,
    hmac_template: HmacSha256,
    api_key_header: HeaderValue,
    passphrase_header: HeaderValue,
}

impl PreparedCreds {
    /// Create prepared credentials from API credentials.
    ///
    /// Pre-computes the HMAC template and header values for fast signing.
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

    /// Sign message using prewarmed HMAC.
    #[inline]
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        let mut mac = self.hmac_template.clone();
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    }

    /// Sign and return base64 (for L2 headers).
    #[inline]
    pub fn sign_b64(&self, message: &[u8]) -> String {
        URL_SAFE.encode(self.sign(message))
    }

    /// Get cached API key header.
    #[inline]
    pub fn api_key_header(&self) -> HeaderValue {
        self.api_key_header.clone()
    }

    /// Get cached passphrase header.
    #[inline]
    pub fn passphrase_header(&self) -> HeaderValue {
        self.passphrase_header.clone()
    }
}

// ============================================================================
// ORDER TYPES
// ============================================================================

/// Order type for Polymarket CLOB.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum PolyOrderType {
    /// Good Till Cancelled (default)
    GTC,
    /// Good Till Time
    GTD,
    /// Fill Or Kill - must fill entirely or cancel
    FOK,
    /// Fill And Kill - fill what you can, cancel rest (aka IOC)
    FAK,
}

impl PolyOrderType {
    /// Get the string representation for API requests.
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
// ORDER RESPONSE TYPES
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
    pub created_at: Option<serde_json::Value>, // Can be string or integer
    #[serde(default)]
    pub expiration: Option<serde_json::Value>, // Can be string or integer
    #[serde(rename = "type")]
    pub order_type: Option<String>,
    pub owner: Option<String>,
}

/// Async fill result from order execution.
#[derive(Debug, Clone)]
pub struct PolyFillAsync {
    pub order_id: String,
    pub filled_size: f64,
    pub fill_cost: f64,
    /// True when Polymarket returns status="delayed" - fill may come later
    pub is_delayed: bool,
}

// ============================================================================
// ORDER CALCULATIONS
// ============================================================================

/// Convert f64 price (0.0-1.0) to basis points (0-10000).
///
/// # Example
/// ```ignore
/// assert_eq!(price_to_bps(0.65), 6500);
/// ```
#[inline(always)]
pub fn price_to_bps(price: f64) -> u64 {
    ((price * 10000.0).round() as i64).max(0) as u64
}

/// Convert f64 size to micro-units (6 decimal places).
///
/// # Example
/// ```ignore
/// assert_eq!(size_to_micro(100.5), 100_500_000);
/// ```
#[inline(always)]
pub fn size_to_micro(size: f64) -> u64 {
    ((size * 1_000_000.0).floor() as i64).max(0) as u64
}

/// BUY order calculation.
///
/// Input: size in micro-units, price in basis points
/// Output: (side=0, maker_amount, taker_amount) in token decimals (6 dp)
///
/// For BUY: taker = size (what we receive), maker = size * price (what we pay)
#[inline(always)]
pub fn get_order_amounts_buy(size_micro: u64, price_bps: u64) -> (i32, u128, u128) {
    let taker = size_micro as u128;
    // maker = size * price / 10000 (convert bps to ratio)
    let maker = (size_micro as u128 * price_bps as u128) / 10000;
    (0, maker, taker)
}

/// SELL order calculation.
///
/// Input: size in micro-units, price in basis points
/// Output: (side=1, maker_amount, taker_amount) in token decimals (6 dp)
///
/// For SELL: maker = size (what we give), taker = size * price (what we receive)
#[inline(always)]
pub fn get_order_amounts_sell(size_micro: u64, price_bps: u64) -> (i32, u128, u128) {
    let maker = size_micro as u128;
    // taker = size * price / 10000 (convert bps to ratio)
    let taker = (size_micro as u128 * price_bps as u128) / 10000;
    (1, maker, taker)
}

/// Validate price is within allowed range for tick=0.01.
///
/// Price must be >= 0.01 (100 bps) and <= 0.99 (9900 bps).
#[inline(always)]
pub fn price_valid(price_bps: u64) -> bool {
    (100..=9900).contains(&price_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_price_to_bps() {
        assert_eq!(price_to_bps(0.65), 6500);
        assert_eq!(price_to_bps(0.01), 100);
        assert_eq!(price_to_bps(0.99), 9900);
        assert_eq!(price_to_bps(0.0), 0);
        assert_eq!(price_to_bps(1.0), 10000);
    }

    #[test]
    fn test_size_to_micro() {
        assert_eq!(size_to_micro(100.5), 100_500_000);
        assert_eq!(size_to_micro(1.0), 1_000_000);
        assert_eq!(size_to_micro(0.000001), 1);
    }

    #[test]
    fn test_get_order_amounts_buy() {
        // Buy 100 shares at 0.65
        let size_micro = size_to_micro(100.0); // 100_000_000
        let price_bps = price_to_bps(0.65); // 6500
        let (side, maker, taker) = get_order_amounts_buy(size_micro, price_bps);

        assert_eq!(side, 0); // BUY side
        assert_eq!(taker, 100_000_000); // We receive 100 shares
        assert_eq!(maker, 65_000_000); // We pay 65 USDC (100 * 0.65)
    }

    #[test]
    fn test_get_order_amounts_sell() {
        // Sell 100 shares at 0.65
        let size_micro = size_to_micro(100.0); // 100_000_000
        let price_bps = price_to_bps(0.65); // 6500
        let (side, maker, taker) = get_order_amounts_sell(size_micro, price_bps);

        assert_eq!(side, 1); // SELL side
        assert_eq!(maker, 100_000_000); // We give 100 shares
        assert_eq!(taker, 65_000_000); // We receive 65 USDC (100 * 0.65)
    }

    #[test]
    fn test_price_valid() {
        assert!(price_valid(100)); // 0.01 - minimum valid
        assert!(price_valid(5000)); // 0.50 - middle
        assert!(price_valid(9900)); // 0.99 - maximum valid
        assert!(!price_valid(99)); // Below minimum
        assert!(!price_valid(9901)); // Above maximum
        assert!(!price_valid(0)); // Zero
        assert!(!price_valid(10000)); // 1.00 - too high
    }

    #[test]
    fn test_poly_order_type_as_str() {
        assert_eq!(PolyOrderType::GTC.as_str(), "GTC");
        assert_eq!(PolyOrderType::GTD.as_str(), "GTD");
        assert_eq!(PolyOrderType::FOK.as_str(), "FOK");
        assert_eq!(PolyOrderType::FAK.as_str(), "FAK");
    }
}
