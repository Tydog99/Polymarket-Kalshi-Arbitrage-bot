//! Configuration and credential management

use anyhow::Result;
use std::env;

use crate::protocol::Platform;

/// Application configuration loaded from environment variables
#[derive(Debug, Clone)]
pub struct Config {
    pub kalshi_api_key: Option<String>,
    pub kalshi_private_key: Option<String>,
    pub polymarket_private_key: Option<String>,
    pub polymarket_api_key: Option<String>,
    pub polymarket_api_secret: Option<String>,
    pub polymarket_api_passphrase: Option<String>,
    pub polymarket_funder: Option<String>,
    /// Polymarket CLOB signature type (0=EOA, 1/2=proxy modes).
    pub polymarket_signature_type: i32,
    pub dry_run: bool,
    /// Which platform this trader is allowed to execute on.
    pub platform: Platform,
    /// WebSocket URL - None means use beacon discovery
    pub websocket_url: Option<String>,
}

impl Config {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        crate::paths::load_dotenv();
        let kalshi_api_key = env::var("KALSHI_API_KEY")
            .or_else(|_| env::var("KALSHI_API_KEY_ID"))
            .ok();
        // Support both direct key content and path to key file
        let kalshi_private_key = env::var("KALSHI_PRIVATE_KEY").ok().or_else(|| {
            env::var("KALSHI_PRIVATE_KEY_PATH")
                .ok()
                .and_then(|path| std::fs::read_to_string(&path).ok())
        });
        // Match controller `.env` naming.
        let polymarket_private_key = env::var("POLY_PRIVATE_KEY").ok();
        let polymarket_api_key = env::var("POLYMARKET_API_KEY").ok();
        let polymarket_api_secret = env::var("POLYMARKET_API_SECRET").ok();
        let polymarket_api_passphrase = env::var("POLYMARKET_API_PASSPHRASE").ok();
        let polymarket_funder = env::var("POLYMARKET_FUNDER")
            .or_else(|_| env::var("POLY_FUNDER"))
            .ok();
        let polymarket_signature_type: i32 = env::var("POLY_SIGNATURE_TYPE")
            .or_else(|_| env::var("POLYMARKET_SIGNATURE_TYPE"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let dry_run = env::var("DRY_RUN")
            .map(|v| v == "1" || v == "true" || v == "yes")
            .unwrap_or(false);

        let platform = match env::var("TRADER_PLATFORM").ok().as_deref() {
            Some("kalshi") => Platform::Kalshi,
            Some("polymarket") => Platform::Polymarket,
            Some(other) => anyhow::bail!("Invalid TRADER_PLATFORM: {}", other),
            None => {
                // Safe-ish fallback: if credentials clearly indicate one platform, pick it.
                // If ambiguous, require explicit configuration so we never accidentally trade both.
                let has_k = kalshi_api_key.is_some() && kalshi_private_key.is_some();
                let has_p = polymarket_private_key.is_some();
                match (has_k, has_p, dry_run) {
                    (true, false, _) => Platform::Kalshi,
                    (false, true, _) => Platform::Polymarket,
                    (false, false, true) => Platform::Kalshi,
                    _ => anyhow::bail!(
                        "TRADER_PLATFORM must be set to 'kalshi' or 'polymarket' (credentials are ambiguous)"
                    ),
                }
            }
        };

        // WEBSOCKET_URL is now optional - if not set, use beacon discovery
        let websocket_url = env::var("WEBSOCKET_URL").ok();

        Ok(Self {
            kalshi_api_key,
            kalshi_private_key,
            polymarket_private_key,
            polymarket_api_key,
            polymarket_api_secret,
            polymarket_api_passphrase,
            polymarket_funder,
            polymarket_signature_type,
            dry_run,
            platform,
            websocket_url,
        })
    }

    /// Check if Kalshi credentials are available
    #[allow(dead_code)]
    pub fn has_kalshi_creds(&self) -> bool {
        self.kalshi_api_key.is_some() && self.kalshi_private_key.is_some()
    }

    /// Check if Polymarket credentials are available
    #[allow(dead_code)]
    pub fn has_polymarket_creds(&self) -> bool {
        self.polymarket_private_key.is_some()
            && (self.polymarket_api_key.is_some() || self.polymarket_funder.is_some())
    }
}
