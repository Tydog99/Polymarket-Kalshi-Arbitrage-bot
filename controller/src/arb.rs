//! Centralized arbitrage detection logic.
//!
//! This module contains the `ArbConfig` struct for configuration and
//! will eventually contain all arbitrage detection and calculation logic.

use crate::types::PriceCents;

/// Configuration for arbitrage detection.
///
/// Loaded from environment variables at startup with sensible defaults.
#[derive(Debug, Clone)]
pub struct ArbConfig {
    /// Threshold in cents below which an arb is considered profitable.
    /// Default: 99 (i.e., combined cost must be <= 99 cents for a 1+ cent profit)
    pub threshold_cents: PriceCents,
    /// Minimum number of contracts to execute.
    /// Default: 1.0
    pub min_contracts: f64,
    /// Kalshi fee rate (7% of notional).
    /// Default: 0.07
    pub kalshi_fee_rate: f64,
    /// Polymarket fee rate (currently 0%).
    /// Default: 0.0
    pub poly_fee_rate: f64,
}

impl ArbConfig {
    /// Load configuration from environment variables.
    ///
    /// Reads:
    /// - `ARB_THRESHOLD_CENTS`: Threshold in cents (default: 99)
    /// - `ARB_MIN_CONTRACTS`: Minimum contracts to execute (default: 1.0)
    pub fn from_env() -> Self {
        let threshold_cents = std::env::var("ARB_THRESHOLD_CENTS")
            .ok()
            .and_then(|s| s.parse::<PriceCents>().ok())
            .unwrap_or(99);

        let min_contracts = std::env::var("ARB_MIN_CONTRACTS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(1.0);

        Self {
            threshold_cents,
            min_contracts,
            kalshi_fee_rate: 0.07,
            poly_fee_rate: 0.0,
        }
    }
}

impl Default for ArbConfig {
    fn default() -> Self {
        Self {
            threshold_cents: 99,
            min_contracts: 1.0,
            kalshi_fee_rate: 0.07,
            poly_fee_rate: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arb_config_defaults() {
        let config = ArbConfig::default();

        assert_eq!(config.threshold_cents, 99);
        assert_eq!(config.min_contracts, 1.0);
        assert_eq!(config.kalshi_fee_rate, 0.07);
        assert_eq!(config.poly_fee_rate, 0.0);
    }

    #[test]
    fn test_arb_config_from_env() {
        // Set environment variables
        std::env::set_var("ARB_THRESHOLD_CENTS", "95");
        std::env::set_var("ARB_MIN_CONTRACTS", "5.0");

        let config = ArbConfig::from_env();

        assert_eq!(config.threshold_cents, 95);
        assert_eq!(config.min_contracts, 5.0);
        // Fee rates should still be defaults
        assert_eq!(config.kalshi_fee_rate, 0.07);
        assert_eq!(config.poly_fee_rate, 0.0);

        // Clean up environment variables
        std::env::remove_var("ARB_THRESHOLD_CENTS");
        std::env::remove_var("ARB_MIN_CONTRACTS");
    }
}
