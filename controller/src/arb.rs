//! Centralized arbitrage configuration and fee calculation.
//!
//! Arb detection logic lives in `ArbOpportunity::detect()`.
//! This module provides:
//! - `ArbConfig` - threshold and min_contracts configuration
//! - `kalshi_fee()` - Kalshi fee calculation

use crate::types::PriceCents;

/// Calculate Kalshi trading fee in cents for a single contract at the given price.
/// Formula: ceil(0.07 * price * (1 - price/100)) in cents
/// Using integer math: (7 * p * (100 - p) + 9999) / 10000
#[inline]
pub fn kalshi_fee(price_cents: PriceCents) -> PriceCents {
    if price_cents == 0 || price_cents >= 100 {
        return 0;
    }
    let p = price_cents as u32;
    ((7 * p * (100 - p) + 9999) / 10000) as PriceCents
}

/// Configuration for arbitrage detection.
///
/// Loaded from environment variables at startup with sensible defaults.
/// Fields are private to enforce invariants; use getter methods for access.
#[derive(Debug, Clone)]
pub struct ArbConfig {
    /// Threshold in cents below which an arb is considered profitable.
    /// Valid range: 1-100 (default: 99)
    threshold_cents: PriceCents,
    /// Minimum number of contracts to execute.
    /// Must be > 0 (default: 1.0)
    min_contracts: f64,
}

impl ArbConfig {
    /// Create a new ArbConfig with validation.
    ///
    /// # Arguments
    /// * `threshold_cents` - Maximum cost for a profitable arb (1-100)
    /// * `min_contracts` - Minimum contracts to execute (must be > 0)
    ///
    /// # Panics
    /// Panics in debug mode if values are out of valid range.
    pub fn new(threshold_cents: PriceCents, min_contracts: f64) -> Self {
        debug_assert!(
            threshold_cents > 0 && threshold_cents <= 100,
            "threshold_cents must be in range 1-100, got {}",
            threshold_cents
        );
        debug_assert!(
            min_contracts > 0.0,
            "min_contracts must be > 0, got {}",
            min_contracts
        );

        Self {
            threshold_cents,
            min_contracts,
        }
    }

    /// Load configuration from environment variables with validation.
    ///
    /// Reads:
    /// - `ARB_THRESHOLD_CENTS`: Threshold in cents (default: 99, valid: 1-100)
    /// - `ARB_MIN_CONTRACTS`: Minimum contracts to execute (default: 1.0, must be > 0)
    ///
    /// Invalid values are logged and replaced with defaults.
    pub fn from_env() -> Self {
        let threshold_cents = std::env::var("ARB_THRESHOLD_CENTS")
            .ok()
            .and_then(|s| s.parse::<PriceCents>().ok())
            .map(|v| {
                if v == 0 || v > 100 {
                    tracing::warn!(
                        "[ARB] ARB_THRESHOLD_CENTS={} out of range (1-100), using default 99",
                        v
                    );
                    99
                } else {
                    v
                }
            })
            .unwrap_or(99);

        let min_contracts = std::env::var("ARB_MIN_CONTRACTS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| {
                if v <= 0.0 {
                    tracing::warn!(
                        "[ARB] ARB_MIN_CONTRACTS={} must be > 0, using default 1.0",
                        v
                    );
                    1.0
                } else {
                    v
                }
            })
            .unwrap_or(1.0);

        Self {
            threshold_cents,
            min_contracts,
        }
    }

    /// Returns the threshold in cents for profitable arbs.
    #[inline]
    pub fn threshold_cents(&self) -> PriceCents {
        self.threshold_cents
    }

    /// Returns the minimum number of contracts required.
    #[inline]
    pub fn min_contracts(&self) -> f64 {
        self.min_contracts
    }
}

impl Default for ArbConfig {
    fn default() -> Self {
        Self {
            threshold_cents: 99,
            min_contracts: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // ArbConfig Tests
    // =========================================================================

    #[test]
    fn test_arb_config_defaults() {
        let config = ArbConfig::default();

        assert_eq!(config.threshold_cents(), 99);
        assert_eq!(config.min_contracts(), 1.0);
    }

    #[test]
    fn test_arb_config_from_env() {
        // Set environment variables
        std::env::set_var("ARB_THRESHOLD_CENTS", "95");
        std::env::set_var("ARB_MIN_CONTRACTS", "5.0");

        let config = ArbConfig::from_env();

        assert_eq!(config.threshold_cents(), 95);
        assert_eq!(config.min_contracts(), 5.0);

        // Clean up environment variables
        std::env::remove_var("ARB_THRESHOLD_CENTS");
        std::env::remove_var("ARB_MIN_CONTRACTS");
    }

    #[test]
    fn test_arb_config_new_with_valid_values() {
        let config = ArbConfig::new(95, 2.5);

        assert_eq!(config.threshold_cents(), 95);
        assert_eq!(config.min_contracts(), 2.5);
    }

    #[test]
    fn test_arb_config_from_env_validates_threshold() {
        // Set invalid threshold (0)
        std::env::set_var("ARB_THRESHOLD_CENTS", "0");
        std::env::remove_var("ARB_MIN_CONTRACTS");

        let config = ArbConfig::from_env();

        // Should fall back to default 99
        assert_eq!(config.threshold_cents(), 99);

        // Clean up
        std::env::remove_var("ARB_THRESHOLD_CENTS");
    }

    #[test]
    fn test_arb_config_from_env_validates_min_contracts() {
        // Set invalid min_contracts (negative)
        std::env::remove_var("ARB_THRESHOLD_CENTS");
        std::env::set_var("ARB_MIN_CONTRACTS", "-1.0");

        let config = ArbConfig::from_env();

        // Should fall back to default 1.0
        assert_eq!(config.min_contracts(), 1.0);

        // Clean up
        std::env::remove_var("ARB_MIN_CONTRACTS");
    }

    // =========================================================================
    // kalshi_fee Tests
    // =========================================================================

    #[test]
    fn test_kalshi_fee_zero_price() {
        assert_eq!(kalshi_fee(0), 0, "Fee should be 0 for price 0");
    }

    #[test]
    fn test_kalshi_fee_price_100() {
        assert_eq!(kalshi_fee(100), 0, "Fee should be 0 for price 100 (no risk)");
    }

    #[test]
    fn test_kalshi_fee_price_above_100() {
        // Prices above 100 are invalid but should still be handled gracefully
        assert_eq!(kalshi_fee(101), 0, "Fee should be 0 for price > 100");
        assert_eq!(kalshi_fee(u16::MAX), 0, "Fee should be 0 for extreme price");
    }

    #[test]
    fn test_kalshi_fee_known_values() {
        // Fee formula: ceil(7 * p * (100 - p) / 10000)
        // At p=50: 7 * 50 * 50 / 10000 = 17500 / 10000 = 1.75 -> ceil = 2
        assert_eq!(kalshi_fee(50), 2);
        // At p=10: 7 * 10 * 90 / 10000 = 6300 / 10000 = 0.63 -> ceil = 1
        assert_eq!(kalshi_fee(10), 1);
        // At p=90: 7 * 90 * 10 / 10000 = 6300 / 10000 = 0.63 -> ceil = 1
        assert_eq!(kalshi_fee(90), 1);
        // At p=1: 7 * 1 * 99 / 10000 = 693 / 10000 = 0.0693 -> ceil = 1
        assert_eq!(kalshi_fee(1), 1);
        // At p=99: 7 * 99 * 1 / 10000 = 693 / 10000 = 0.0693 -> ceil = 1
        assert_eq!(kalshi_fee(99), 1);
    }
}
