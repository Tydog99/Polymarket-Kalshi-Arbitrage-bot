//! Kalshi configuration.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use pkcs1::DecodeRsaPrivateKey;
use rsa::{
    pss::SigningKey,
    sha2::Sha256,
    signature::{RandomizedSigner, SignatureEncoding},
    RsaPrivateKey,
};
use std::path::PathBuf;

// === API Constants ===

pub const KALSHI_API_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
pub const KALSHI_WS_URL: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";
pub const KALSHI_API_DELAY_MS: u64 = 60;

// === Kalshi Auth Config ===

pub struct KalshiConfig {
    pub api_key_id: String,
    pub private_key: RsaPrivateKey,
}

impl KalshiConfig {
    /// Load configuration from environment variables.
    ///
    /// Expects env vars to already be loaded (e.g., via dotenv by the caller).
    /// Required: `KALSHI_API_KEY_ID`
    /// Optional: `KALSHI_PRIVATE_KEY_PATH` or `KALSHI_PRIVATE_KEY_FILE` (defaults to "kalshi_private_key.txt")
    pub fn from_env() -> Result<Self> {
        let api_key_id =
            std::env::var("KALSHI_API_KEY_ID").context("KALSHI_API_KEY_ID not set")?;

        // Support both KALSHI_PRIVATE_KEY_PATH and KALSHI_PRIVATE_KEY_FILE for compatibility
        let key_path = std::env::var("KALSHI_PRIVATE_KEY_PATH")
            .or_else(|_| std::env::var("KALSHI_PRIVATE_KEY_FILE"))
            .unwrap_or_else(|_| "kalshi_private_key.txt".to_string());

        let resolved_key_path = resolve_path(&key_path);
        let private_key_pem = std::fs::read_to_string(&resolved_key_path)
            .with_context(|| {
                format!(
                    "Failed to read private key from {} (resolved to {})",
                    key_path,
                    resolved_key_path.display()
                )
            })?
            .trim()
            .to_owned();

        let private_key = RsaPrivateKey::from_pkcs1_pem(&private_key_pem)
            .context("Failed to parse private key PEM")?;

        Ok(Self {
            api_key_id,
            private_key,
        })
    }

    pub fn sign(&self, message: &str) -> Result<String> {
        tracing::debug!("[KALSHI-DEBUG] Signing message: {}", message);
        let signing_key = SigningKey::<Sha256>::new(self.private_key.clone());
        let signature = signing_key.sign_with_rng(&mut rand::thread_rng(), message.as_bytes());
        let sig_b64 = BASE64.encode(signature.to_bytes());
        tracing::debug!(
            "[KALSHI-DEBUG] Signature (first 50 chars): {}...",
            &sig_b64[..50.min(sig_b64.len())]
        );
        Ok(sig_b64)
    }
}

/// Resolve a path, expanding ~ to home directory if present.
fn resolve_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}
