//! Centralized arbitrage detection logic.
//!
//! This module contains the `ArbConfig` struct for configuration and
//! will eventually contain all arbitrage detection and calculation logic.

use crate::types::{ArbType, PriceCents, SizeCents};

/// Calculate Kalshi trading fee in cents for a single contract at the given price.
/// Formula: ceil(0.07 * price * (1 - price/100)) in cents
/// Using integer math: (7 * p * (100 - p) + 9999) / 10000
#[inline]
fn kalshi_fee(price_cents: PriceCents) -> PriceCents {
    if price_cents == 0 || price_cents >= 100 {
        return 0;
    }
    let p = price_cents as u32;
    ((7 * p * (100 - p) + 9999) / 10000) as PriceCents
}

/// Encapsulates an arbitrage opportunity with detection logic.
///
/// Construct with raw orderbook data from both platforms; the struct
/// calculates costs for all 4 arb types (with fees) and determines
/// if any valid opportunity exists.
#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    market_id: u16,
    arb_type: Option<ArbType>,
    yes_price: PriceCents,
    no_price: PriceCents,
    yes_size: SizeCents,
    no_size: SizeCents,
    detected_ns: u64,
}

impl ArbOpportunity {
    /// Create a new ArbOpportunity by analyzing orderbook data from both platforms.
    ///
    /// # Arguments
    /// * `market_id` - Unique market identifier
    /// * `kalshi` - Tuple of (yes_ask, no_ask, yes_size, no_size) from Kalshi
    /// * `poly` - Tuple of (yes_ask, no_ask, yes_size, no_size) from Polymarket
    /// * `config` - Arb detection configuration
    /// * `detected_ns` - Detection timestamp in nanoseconds
    ///
    /// # Returns
    /// An `ArbOpportunity` with `arb_type` set to None if no valid arb exists.
    pub fn new(
        market_id: u16,
        kalshi: (PriceCents, PriceCents, SizeCents, SizeCents),
        poly: (PriceCents, PriceCents, SizeCents, SizeCents),
        config: &ArbConfig,
        detected_ns: u64,
    ) -> Self {
        let (k_yes, k_no, k_yes_sz, k_no_sz) = kalshi;
        let (p_yes, p_no, p_yes_sz, p_no_sz) = poly;

        // Check for invalid prices (0 = no price available)
        if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
            return Self {
                market_id,
                arb_type: None,
                yes_price: 0,
                no_price: 0,
                yes_size: 0,
                no_size: 0,
                detected_ns,
            };
        }

        // Pre-compute Kalshi fees
        let k_yes_fee = kalshi_fee(k_yes);
        let k_no_fee = kalshi_fee(k_no);

        let threshold = config.threshold_cents;
        let min_contracts = config.min_contracts;

        // Priority order: PolyYesKalshiNo, KalshiYesPolyNo, PolyOnly, KalshiOnly
        // Check each arb type for profitability AND sufficient size

        // 1. PolyYesKalshiNo: Buy Poly YES + Kalshi NO
        let cost1 = p_yes + k_no + k_no_fee;
        let yes_contracts1 = if p_yes > 0 { p_yes_sz / p_yes } else { 0 };
        let no_contracts1 = if k_no > 0 { k_no_sz / k_no } else { 0 };
        let max_contracts1 = yes_contracts1.min(no_contracts1);
        if cost1 <= threshold && max_contracts1 as f64 >= min_contracts {
            return Self {
                market_id,
                arb_type: Some(ArbType::PolyYesKalshiNo),
                yes_price: p_yes,
                no_price: k_no,
                yes_size: p_yes_sz,
                no_size: k_no_sz,
                detected_ns,
            };
        }

        // 2. KalshiYesPolyNo: Buy Kalshi YES + Poly NO
        let cost2 = k_yes + k_yes_fee + p_no;
        let yes_contracts2 = if k_yes > 0 { k_yes_sz / k_yes } else { 0 };
        let no_contracts2 = if p_no > 0 { p_no_sz / p_no } else { 0 };
        let max_contracts2 = yes_contracts2.min(no_contracts2);
        if cost2 <= threshold && max_contracts2 as f64 >= min_contracts {
            return Self {
                market_id,
                arb_type: Some(ArbType::KalshiYesPolyNo),
                yes_price: k_yes,
                no_price: p_no,
                yes_size: k_yes_sz,
                no_size: p_no_sz,
                detected_ns,
            };
        }

        // 3. PolyOnly: Buy Poly YES + Poly NO (no fees)
        let cost3 = p_yes + p_no;
        let yes_contracts3 = if p_yes > 0 { p_yes_sz / p_yes } else { 0 };
        let no_contracts3 = if p_no > 0 { p_no_sz / p_no } else { 0 };
        let max_contracts3 = yes_contracts3.min(no_contracts3);
        if cost3 <= threshold && max_contracts3 as f64 >= min_contracts {
            return Self {
                market_id,
                arb_type: Some(ArbType::PolyOnly),
                yes_price: p_yes,
                no_price: p_no,
                yes_size: p_yes_sz,
                no_size: p_no_sz,
                detected_ns,
            };
        }

        // 4. KalshiOnly: Buy Kalshi YES + Kalshi NO (double fees)
        let cost4 = k_yes + k_yes_fee + k_no + k_no_fee;
        let yes_contracts4 = if k_yes > 0 { k_yes_sz / k_yes } else { 0 };
        let no_contracts4 = if k_no > 0 { k_no_sz / k_no } else { 0 };
        let max_contracts4 = yes_contracts4.min(no_contracts4);
        if cost4 <= threshold && max_contracts4 as f64 >= min_contracts {
            return Self {
                market_id,
                arb_type: Some(ArbType::KalshiOnly),
                yes_price: k_yes,
                no_price: k_no,
                yes_size: k_yes_sz,
                no_size: k_no_sz,
                detected_ns,
            };
        }

        // No valid arb found
        Self {
            market_id,
            arb_type: None,
            yes_price: 0,
            no_price: 0,
            yes_size: 0,
            no_size: 0,
            detected_ns,
        }
    }

    /// Returns true if a valid arbitrage opportunity was detected.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.arb_type.is_some()
    }

    /// Returns the detected arbitrage type, or None if no valid arb.
    #[inline]
    pub fn arb_type(&self) -> Option<ArbType> {
        self.arb_type
    }

    /// Returns the market identifier.
    #[inline]
    pub fn market_id(&self) -> u16 {
        self.market_id
    }

    /// Returns the YES side price in cents.
    #[inline]
    pub fn yes_price(&self) -> PriceCents {
        self.yes_price
    }

    /// Returns the NO side price in cents.
    #[inline]
    pub fn no_price(&self) -> PriceCents {
        self.no_price
    }

    /// Returns the YES side available size in cents.
    #[inline]
    pub fn yes_size(&self) -> SizeCents {
        self.yes_size
    }

    /// Returns the NO side available size in cents.
    #[inline]
    pub fn no_size(&self) -> SizeCents {
        self.no_size
    }

    /// Returns the detection timestamp in nanoseconds.
    #[inline]
    pub fn detected_ns(&self) -> u64 {
        self.detected_ns
    }

    /// Returns the maximum number of contracts that can be executed.
    /// This is the minimum of (yes_size / yes_price) and (no_size / no_price).
    #[inline]
    pub fn max_contracts(&self) -> u16 {
        if self.yes_price == 0 || self.no_price == 0 {
            return 0;
        }
        let yes_contracts = self.yes_size / self.yes_price;
        let no_contracts = self.no_size / self.no_price;
        yes_contracts.min(no_contracts)
    }

    /// Returns the gross profit per contract in cents (100 - cost - fees).
    #[inline]
    pub fn gross_profit_cents(&self) -> i16 {
        if !self.is_valid() {
            return 0;
        }

        let fee = match self.arb_type {
            Some(ArbType::PolyYesKalshiNo) => kalshi_fee(self.no_price),
            Some(ArbType::KalshiYesPolyNo) => kalshi_fee(self.yes_price),
            Some(ArbType::PolyOnly) => 0,
            Some(ArbType::KalshiOnly) => kalshi_fee(self.yes_price) + kalshi_fee(self.no_price),
            None => 0,
        };

        100 - (self.yes_price as i16 + self.no_price as i16 + fee as i16)
    }
}

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
    use crate::types::ArbType;

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

    // =========================================================================
    // ArbOpportunity Tests
    // =========================================================================

    #[test]
    fn test_arb_opportunity_no_arb_when_prices_too_high() {
        // Prices sum to 100 - no arb possible
        let config = ArbConfig::default();
        let kalshi = (50u16, 50u16, 1000u16, 1000u16); // yes_ask, no_ask, yes_size, no_size
        let poly = (50u16, 50u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 12345);

        assert!(!opp.is_valid(), "Should not detect arb when prices sum to 100");
        assert!(opp.arb_type().is_none());
    }

    #[test]
    fn test_arb_opportunity_detects_poly_yes_kalshi_no() {
        // Poly YES 40 + Kalshi NO 50 = 90 raw
        // Kalshi fee on 50c = 2c
        // Total = 92c < 99c threshold -> valid arb
        let config = ArbConfig::default();
        let kalshi = (55u16, 50u16, 1000u16, 1000u16);
        let poly = (40u16, 65u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(42, kalshi, poly, &config, 12345);

        assert!(opp.is_valid(), "Should detect PolyYesKalshiNo arb");
        assert_eq!(opp.arb_type(), Some(ArbType::PolyYesKalshiNo));
        assert_eq!(opp.market_id(), 42);
        assert_eq!(opp.yes_price(), 40); // Poly YES price
        assert_eq!(opp.no_price(), 50); // Kalshi NO price
        assert_eq!(opp.detected_ns(), 12345);
    }

    #[test]
    fn test_arb_opportunity_invalid_when_size_zero() {
        // Valid prices but zero size should be invalid
        let config = ArbConfig::default();
        let kalshi = (55u16, 50u16, 0u16, 0u16); // zero size
        let poly = (40u16, 65u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 12345);

        assert!(!opp.is_valid(), "Should be invalid when size is zero");
    }

    #[test]
    fn test_arb_opportunity_max_contracts() {
        // Poly YES 40c with 500c size = 500/40 = 12.5 -> 12 contracts
        // Kalshi NO 50c with 1000c size = 1000/50 = 20 contracts
        // max_contracts = min(12, 20) = 12
        let config = ArbConfig::default();
        let kalshi = (55u16, 50u16, 1000u16, 1000u16);
        let poly = (40u16, 65u16, 500u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 0);

        assert!(opp.is_valid());
        assert_eq!(opp.max_contracts(), 12);
        assert_eq!(opp.yes_size(), 500);
        assert_eq!(opp.no_size(), 1000);
    }

    #[test]
    fn test_arb_opportunity_gross_profit_cents() {
        // Poly YES 40 + Kalshi NO 50 = 90 raw
        // Kalshi fee on 50c = 2c
        // Total cost = 92c
        // Gross profit = 100 - 92 = 8c per contract
        let config = ArbConfig::default();
        let kalshi = (55u16, 50u16, 1000u16, 1000u16);
        let poly = (40u16, 65u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 0);

        assert!(opp.is_valid());
        assert_eq!(opp.gross_profit_cents(), 8);
    }

    #[test]
    fn test_arb_opportunity_invalid_price_zero() {
        // Price of 0 means no price available
        let config = ArbConfig::default();
        let kalshi = (0u16, 50u16, 1000u16, 1000u16); // yes price = 0
        let poly = (40u16, 65u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 0);

        assert!(!opp.is_valid(), "Should be invalid when price is 0");
    }

    #[test]
    fn test_arb_opportunity_kalshi_yes_poly_no() {
        // Kalshi YES 40 + Poly NO 50 = 90 raw
        // Kalshi fee on 40c = 2c
        // Total = 92c < 99c threshold -> valid arb
        let config = ArbConfig::default();
        let kalshi = (40u16, 65u16, 1000u16, 1000u16);
        let poly = (55u16, 50u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 0);

        assert!(opp.is_valid());
        assert_eq!(opp.arb_type(), Some(ArbType::KalshiYesPolyNo));
        assert_eq!(opp.yes_price(), 40); // Kalshi YES price
        assert_eq!(opp.no_price(), 50); // Poly NO price
    }

    #[test]
    fn test_arb_opportunity_poly_only() {
        // Poly YES 40 + Poly NO 48 = 88c (no fees)
        // < 99c threshold -> valid arb
        // Both cross-platform arbs should NOT be valid here
        let config = ArbConfig::default();
        let kalshi = (60u16, 60u16, 1000u16, 1000u16); // expensive
        let poly = (40u16, 48u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 0);

        assert!(opp.is_valid());
        assert_eq!(opp.arb_type(), Some(ArbType::PolyOnly));
    }

    #[test]
    fn test_arb_opportunity_kalshi_only() {
        // Kalshi YES 40 + Kalshi NO 40 = 80 raw
        // Kalshi fee on both: 2 + 2 = 4c
        // Total = 84c < 99c threshold -> valid arb
        // But cross-platform and poly-only should not be valid
        let config = ArbConfig::default();
        let kalshi = (40u16, 40u16, 1000u16, 1000u16);
        let poly = (60u16, 60u16, 1000u16, 1000u16); // expensive

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 0);

        assert!(opp.is_valid());
        assert_eq!(opp.arb_type(), Some(ArbType::KalshiOnly));
    }

    #[test]
    fn test_arb_opportunity_priority_order() {
        // All 4 arb types are valid - should pick PolyYesKalshiNo (priority)
        let config = ArbConfig::default();
        let kalshi = (40u16, 40u16, 1000u16, 1000u16);
        let poly = (40u16, 40u16, 1000u16, 1000u16);

        let opp = ArbOpportunity::new(0, kalshi, poly, &config, 0);

        assert!(opp.is_valid());
        // Priority: PolyYesKalshiNo first
        assert_eq!(opp.arb_type(), Some(ArbType::PolyYesKalshiNo));
    }
}
