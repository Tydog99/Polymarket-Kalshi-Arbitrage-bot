//! Centralized arbitrage configuration and fee calculation.
//!
//! Arb detection logic lives in `ArbOpportunity::detect()`.
//! This module provides:
//! - `ArbConfig` - threshold and min_contracts configuration
//! - `kalshi_fee()` - Kalshi fee calculation

use crate::types::{ArbOpportunity, ArbType, OrderbookDepth, PriceCents, SizeCents, DEPTH_LEVELS};

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

// ============================================================================
// EV-Maximizing Depth-Aware Arb Detection
// ============================================================================

/// Detect the arbitrage opportunity with the highest expected value (EV)
/// by considering all price levels in the orderbook depth.
///
/// Unlike `ArbOpportunity::detect()` which only looks at top-of-book,
/// this function examines all combinations of price levels to find the
/// opportunity that maximizes EV = |gap| * contracts.
///
/// # Arguments
/// * `market_id` - Unique identifier for this market
/// * `kalshi` - Kalshi orderbook depth (3 levels each side)
/// * `poly` - Polymarket orderbook depth (3 levels each side)
/// * `config` - Arb configuration (threshold, min_contracts)
/// * `detected_ns` - Detection timestamp in nanoseconds
///
/// # Returns
/// The `ArbOpportunity` with highest EV if one exists, `None` otherwise.
pub fn detect_best_ev(
    market_id: u16,
    kalshi: &OrderbookDepth,
    poly: &OrderbookDepth,
    config: &ArbConfig,
    detected_ns: u64,
) -> Option<ArbOpportunity> {
    let mut best_ev: i64 = 0;
    let mut best_opp: Option<ArbOpportunity> = None;

    // Iterate all 81 combinations (3^4) of price levels
    for k_yes_level in 0..DEPTH_LEVELS {
        let k_yes = &kalshi.yes_levels[k_yes_level];
        if k_yes.price == 0 {
            continue;
        }

        for k_no_level in 0..DEPTH_LEVELS {
            let k_no = &kalshi.no_levels[k_no_level];
            if k_no.price == 0 {
                continue;
            }

            for p_yes_level in 0..DEPTH_LEVELS {
                let p_yes = &poly.yes_levels[p_yes_level];
                if p_yes.price == 0 {
                    continue;
                }

                for p_no_level in 0..DEPTH_LEVELS {
                    let p_no = &poly.no_levels[p_no_level];
                    if p_no.price == 0 {
                        continue;
                    }

                    // Calculate Kalshi fees
                    let k_yes_fee = kalshi_fee(k_yes.price);
                    let k_no_fee = kalshi_fee(k_no.price);

                    // Evaluate all 4 arb types and pick the one with best EV
                    let candidates: [(
                        ArbType,
                        PriceCents,
                        PriceCents,
                        SizeCents,
                        SizeCents,
                        SizeCents,
                    ); 4] = [
                        // PolyYesKalshiNo: buy Poly YES + Kalshi NO
                        (
                            ArbType::PolyYesKalshiNo,
                            p_yes.price + k_no.price + k_no_fee, // cost
                            p_yes.price,
                            k_no.price,
                            p_yes.size,
                            k_no.size,
                        ),
                        // KalshiYesPolyNo: buy Kalshi YES + Poly NO
                        (
                            ArbType::KalshiYesPolyNo,
                            k_yes.price + k_yes_fee + p_no.price, // cost
                            k_yes.price,
                            p_no.price,
                            k_yes.size,
                            p_no.size,
                        ),
                        // PolyOnly: buy Poly YES + Poly NO
                        (
                            ArbType::PolyOnly,
                            p_yes.price + p_no.price, // cost (no fees)
                            p_yes.price,
                            p_no.price,
                            p_yes.size,
                            p_no.size,
                        ),
                        // KalshiOnly: buy Kalshi YES + Kalshi NO
                        (
                            ArbType::KalshiOnly,
                            k_yes.price + k_yes_fee + k_no.price + k_no_fee, // cost
                            k_yes.price,
                            k_no.price,
                            k_yes.size,
                            k_no.size,
                        ),
                    ];

                    for (arb_type, cost, yes_price, no_price, yes_size, no_size) in candidates {
                        // Gap = cost - 100; negative means profit
                        let gap = cost as i64 - 100;

                        // Skip if not profitable (gap must be < 0)
                        if gap >= 0 {
                            continue;
                        }

                        // Also check against threshold (cost must be <= threshold)
                        if cost > config.threshold_cents() {
                            continue;
                        }

                        // Calculate max contracts: min of contracts purchasable on each side
                        let yes_contracts = if yes_price > 0 {
                            yes_size / yes_price
                        } else {
                            0
                        };
                        let no_contracts = if no_price > 0 { no_size / no_price } else { 0 };
                        let contracts = yes_contracts.min(no_contracts);

                        // Skip if insufficient contracts
                        if (contracts as f64) < config.min_contracts() {
                            continue;
                        }

                        // EV = |gap| * contracts
                        let ev = gap.abs() * (contracts as i64);

                        if ev > best_ev {
                            best_ev = ev;
                            best_opp = Some(ArbOpportunity {
                                market_id,
                                arb_type,
                                yes_price,
                                no_price,
                                yes_size,
                                no_size,
                                detected_ns,
                                is_test: false,
                            });
                        }
                    }
                }
            }
        }
    }

    best_opp
}

// ============================================================================
// Startup Sweep
// ============================================================================

use crate::config;
use crate::types::{GlobalState, MarketPair};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Result of a market sweep operation.
#[derive(Debug, Default)]
pub struct SweepResult {
    pub markets_scanned: usize,
    pub arbs_found: usize,
    pub routed_to_confirm: usize,
    pub routed_to_exec: usize,
}

/// Sweep all markets for arbitrage opportunities and route to appropriate channels.
///
/// This is the core logic extracted from the startup sweep for testability.
/// Routes arbs based on `config::requires_confirmation()` for each league.
pub fn sweep_markets(
    state: &GlobalState,
    clock_ns: u64,
    exec_tx: &mpsc::Sender<ArbOpportunity>,
    confirm_tx: &mpsc::Sender<(ArbOpportunity, Arc<MarketPair>)>,
) -> SweepResult {
    let mut result = SweepResult::default();
    let market_count = state.market_count();

    for market in state.markets.iter().take(market_count) {
        let (k_yes, k_no, _, _) = market.kalshi.read().top_of_book();
        let (p_yes, p_no, _, _) = market.poly.read().top_of_book();

        // Only check markets with both platforms populated
        if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
            continue;
        }
        result.markets_scanned += 1;

        // Check for arbs using ArbOpportunity::detect()
        if let Some(req) = ArbOpportunity::detect(
            market.market_id,
            market.kalshi.read().top_of_book(),
            market.poly.read().top_of_book(),
            state.arb_config(),
            clock_ns,
        ) {
            result.arbs_found += 1;

            // Route based on confirmation requirement
            if let Some(pair) = market.pair() {
                if config::requires_confirmation(&pair.league) {
                    if confirm_tx.try_send((req, pair)).is_ok() {
                        result.routed_to_confirm += 1;
                    }
                } else if exec_tx.try_send(req).is_ok() {
                    result.routed_to_exec += 1;
                }
            } else if exec_tx.try_send(req).is_ok() {
                // No pair metadata - fall back to direct execution
                result.routed_to_exec += 1;
            }
        }
    }

    result
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
    fn test_arb_config_new_with_valid_values() {
        let config = ArbConfig::new(95, 2.5);

        assert_eq!(config.threshold_cents(), 95);
        assert_eq!(config.min_contracts(), 2.5);
    }

    // Note: env var tests are commented out due to parallel test execution issues.
    // These tests work correctly when run with `cargo test -- --test-threads=1`
    // but fail when run in parallel because env vars are process-global.
    //
    // The from_env() functionality is tested implicitly in integration tests
    // where the entire process can control its environment.

    // =========================================================================
    // kalshi_fee Tests
    // =========================================================================

    #[test]
    fn test_kalshi_fee_zero_price() {
        assert_eq!(kalshi_fee(0), 0, "Fee should be 0 for price 0");
    }

    #[test]
    fn test_kalshi_fee_price_100() {
        assert_eq!(
            kalshi_fee(100),
            0,
            "Fee should be 0 for price 100 (no risk)"
        );
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

    // =========================================================================
    // detect_best_ev Tests
    // =========================================================================

    use crate::types::PriceLevel;

    /// Helper to create an OrderbookDepth with specified price levels
    fn make_depth(yes: [(u16, u16); 3], no: [(u16, u16); 3]) -> OrderbookDepth {
        OrderbookDepth {
            yes_levels: [
                PriceLevel {
                    price: yes[0].0,
                    size: yes[0].1,
                },
                PriceLevel {
                    price: yes[1].0,
                    size: yes[1].1,
                },
                PriceLevel {
                    price: yes[2].0,
                    size: yes[2].1,
                },
            ],
            no_levels: [
                PriceLevel {
                    price: no[0].0,
                    size: no[0].1,
                },
                PriceLevel {
                    price: no[1].0,
                    size: no[1].1,
                },
                PriceLevel {
                    price: no[2].0,
                    size: no[2].1,
                },
            ],
        }
    }

    #[test]
    fn test_detect_best_ev_returns_none_when_no_profitable_arb() {
        let config = ArbConfig::default();

        // All prices sum to > 100 after fees, no arb possible
        let kalshi = make_depth(
            [(55, 1000), (56, 1000), (57, 1000)],
            [(55, 1000), (56, 1000), (57, 1000)],
        );
        let poly = make_depth(
            [(55, 1000), (56, 1000), (57, 1000)],
            [(55, 1000), (56, 1000), (57, 1000)],
        );

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        assert!(
            result.is_none(),
            "Should return None when no profitable arb exists"
        );
    }

    #[test]
    fn test_detect_best_ev_finds_arb_at_top_of_book() {
        let config = ArbConfig::default();

        // Poly YES @ 45 + Kalshi NO @ 52 = 97 + ~2c fee = ~99 (profitable)
        let kalshi = make_depth([(55, 1000), (0, 0), (0, 0)], [(52, 1000), (0, 0), (0, 0)]);
        let poly = make_depth([(45, 1000), (0, 0), (0, 0)], [(58, 1000), (0, 0), (0, 0)]);

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find arb at top of book");

        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo);
        assert_eq!(arb.yes_price, 45);
        assert_eq!(arb.no_price, 52);
    }

    #[test]
    fn test_detect_best_ev_chooses_higher_ev_deeper_in_book() {
        let config = ArbConfig::default();

        // Level 0: Poly YES @ 45 + Kalshi NO @ 53 = 98 + 2c fee = 100 (NOT profitable)
        // Level 1: Poly YES @ 44 + Kalshi NO @ 52 = 96 + 2c fee = 98 (2c profit)
        // Level 2: Poly YES @ 43 + Kalshi NO @ 51 = 94 + 2c fee = 96 (4c profit)
        //
        // Sizes at level 0: 100 cents each
        // Sizes at level 1: 500 cents each
        // Sizes at level 2: 2000 cents each
        //
        // Level 0: gap = 0, EV = 0
        // Level 1: gap = -2, contracts = min(500/44, 500/52) = min(11, 9) = 9, EV = 2 * 9 = 18
        // Level 2: gap = -4, contracts = min(2000/43, 2000/51) = min(46, 39) = 39, EV = 4 * 39 = 156
        //
        // Should pick level 2 with EV=156

        let kalshi = make_depth(
            [(60, 100), (0, 0), (0, 0)], // Kalshi YES expensive (won't be used)
            [(53, 100), (52, 500), (51, 2000)],
        );
        let poly = make_depth(
            [(45, 100), (44, 500), (43, 2000)],
            [(60, 100), (0, 0), (0, 0)], // Poly NO expensive (won't be used)
        );

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find best EV arb");

        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo);
        assert_eq!(arb.yes_price, 43, "Should pick level 2 Poly YES");
        assert_eq!(arb.no_price, 51, "Should pick level 2 Kalshi NO");
        assert_eq!(arb.yes_size, 2000);
        assert_eq!(arb.no_size, 2000);
    }

    #[test]
    fn test_detect_best_ev_prefers_higher_ev_over_better_gap() {
        let config = ArbConfig::default();

        // Scenario: Better gap at one level, but much higher size (and thus EV) at another
        //
        // Level 0: Poly YES @ 40 + Kalshi NO @ 50 = 90 + 2c fee = 92 (8c profit)
        //          Size: 50 cents each -> contracts = min(50/40, 50/50) = min(1, 1) = 1
        //          EV = 8 * 1 = 8
        //
        // Level 1: Poly YES @ 44 + Kalshi NO @ 52 = 96 + 2c fee = 98 (2c profit)
        //          Size: 1000 cents each -> contracts = min(1000/44, 1000/52) = min(22, 19) = 19
        //          EV = 2 * 19 = 38
        //
        // Should pick level 1 with EV=38 despite worse gap

        let kalshi = make_depth([(60, 100), (0, 0), (0, 0)], [(50, 50), (52, 1000), (0, 0)]);
        let poly = make_depth([(40, 50), (44, 1000), (0, 0)], [(60, 100), (0, 0), (0, 0)]);

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find best EV arb");

        // Check that we picked the higher EV option (level 1)
        assert_eq!(arb.yes_price, 44, "Should prefer level 1 with higher EV");
        assert_eq!(arb.no_price, 52);
    }

    #[test]
    fn test_detect_best_ev_respects_min_contracts() {
        // Require at least 5 contracts
        let config = ArbConfig::new(99, 5.0);

        // Level 0: Great gap but only 2 contracts possible (below min)
        // Level 1: Smaller gap but 10 contracts possible (above min)

        let kalshi = make_depth(
            [(60, 100), (0, 0), (0, 0)],
            [(50, 100), (54, 1000), (0, 0)], // 100/50=2 contracts, 1000/54=18 contracts
        );
        let poly = make_depth(
            [(40, 80), (43, 1000), (0, 0)], // 80/40=2 contracts, 1000/43=23 contracts
            [(60, 100), (0, 0), (0, 0)],
        );

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find arb meeting min_contracts");

        // Level 0: 40 + 50 + 2c fee = 92, but only 2 contracts (rejected)
        // Level 1: 43 + 54 + 2c fee = 99, contracts = min(23, 18) = 18 >= 5 (accepted)
        assert_eq!(arb.yes_price, 43);
        assert_eq!(arb.no_price, 54);
    }

    #[test]
    fn test_detect_best_ev_skips_zero_price_levels() {
        let config = ArbConfig::default();

        // Only level 1 has valid prices on all sides
        let kalshi = make_depth([(0, 0), (55, 500), (0, 0)], [(0, 0), (52, 500), (0, 0)]);
        let poly = make_depth([(0, 0), (45, 500), (0, 0)], [(0, 0), (58, 500), (0, 0)]);

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find arb at level 1");

        assert_eq!(arb.yes_price, 45);
        assert_eq!(arb.no_price, 52);
    }

    #[test]
    fn test_detect_best_ev_finds_kalshi_yes_poly_no() {
        let config = ArbConfig::default();

        // Make PolyYesKalshiNo unprofitable, but KalshiYesPolyNo profitable
        let kalshi = make_depth(
            [(43, 1000), (0, 0), (0, 0)], // Kalshi YES cheap
            [(60, 1000), (0, 0), (0, 0)], // Kalshi NO expensive
        );
        let poly = make_depth(
            [(60, 1000), (0, 0), (0, 0)], // Poly YES expensive
            [(54, 1000), (0, 0), (0, 0)], // Poly NO cheap
        );

        // KalshiYesPolyNo: 43 + 2c fee + 54 = 99 <= 99 (1c profit)
        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find KalshiYesPolyNo arb");

        assert_eq!(arb.arb_type, ArbType::KalshiYesPolyNo);
        assert_eq!(arb.yes_price, 43);
        assert_eq!(arb.no_price, 54);
    }

    #[test]
    fn test_detect_best_ev_finds_poly_only() {
        let config = ArbConfig::default();

        // Make cross-platform arbs unprofitable, but PolyOnly profitable
        let kalshi = make_depth([(60, 1000), (0, 0), (0, 0)], [(60, 1000), (0, 0), (0, 0)]);
        let poly = make_depth(
            [(40, 1000), (0, 0), (0, 0)], // Poly YES 40
            [(48, 1000), (0, 0), (0, 0)], // Poly NO 48
        );

        // PolyOnly: 40 + 48 = 88 < 99 (12c profit, no fees)
        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find PolyOnly arb");

        assert_eq!(arb.arb_type, ArbType::PolyOnly);
        assert_eq!(arb.yes_price, 40);
        assert_eq!(arb.no_price, 48);
    }

    #[test]
    fn test_detect_best_ev_finds_kalshi_only() {
        let config = ArbConfig::default();

        // Make Poly expensive, Kalshi both sides cheap
        let kalshi = make_depth(
            [(40, 1000), (0, 0), (0, 0)], // Kalshi YES 40
            [(40, 1000), (0, 0), (0, 0)], // Kalshi NO 40
        );
        let poly = make_depth([(60, 1000), (0, 0), (0, 0)], [(60, 1000), (0, 0), (0, 0)]);

        // KalshiOnly: 40 + 2c fee + 40 + 2c fee = 84 < 99 (16c profit)
        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find KalshiOnly arb");

        assert_eq!(arb.arb_type, ArbType::KalshiOnly);
        assert_eq!(arb.yes_price, 40);
        assert_eq!(arb.no_price, 40);
    }

    #[test]
    fn test_detect_best_ev_preserves_market_id_and_timestamp() {
        let config = ArbConfig::default();

        let kalshi = make_depth([(60, 1000), (0, 0), (0, 0)], [(52, 1000), (0, 0), (0, 0)]);
        let poly = make_depth([(45, 1000), (0, 0), (0, 0)], [(60, 1000), (0, 0), (0, 0)]);

        let result = detect_best_ev(42, &kalshi, &poly, &config, 999888777);
        let arb = result.expect("Should find arb");

        assert_eq!(arb.market_id, 42);
        assert_eq!(arb.detected_ns, 999888777);
        assert!(!arb.is_test);
    }

    #[test]
    fn test_detect_best_ev_mixed_levels_cross_platform() {
        let config = ArbConfig::default();

        // Best arb is Poly YES level 2 + Kalshi NO level 1
        // Level 0 prices are too expensive
        // Level 1 Kalshi NO is cheap with good size
        // Level 2 Poly YES is cheap with good size

        let kalshi = make_depth(
            [(60, 100), (0, 0), (0, 0)],        // Kalshi YES expensive
            [(55, 100), (51, 2000), (53, 500)], // Kalshi NO: level 1 best
        );
        let poly = make_depth(
            [(48, 100), (46, 500), (43, 2000)], // Poly YES: level 2 best
            [(60, 100), (0, 0), (0, 0)],        // Poly NO expensive
        );

        // Best combo: Poly YES 43 (level 2) + Kalshi NO 51 (level 1)
        // Cost: 43 + 51 + 2c fee = 96, profit = 4c
        // Contracts: min(2000/43, 2000/51) = min(46, 39) = 39
        // EV = 4 * 39 = 156

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find mixed level arb");

        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo);
        assert_eq!(arb.yes_price, 43, "Should pick Poly YES from level 2");
        assert_eq!(arb.no_price, 51, "Should pick Kalshi NO from level 1");
    }

    #[test]
    fn test_detect_best_ev_all_prices_zero_returns_none() {
        let config = ArbConfig::default();

        let kalshi = make_depth([(0, 0), (0, 0), (0, 0)], [(0, 0), (0, 0), (0, 0)]);
        let poly = make_depth([(0, 0), (0, 0), (0, 0)], [(0, 0), (0, 0), (0, 0)]);

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        assert!(result.is_none(), "Should return None when all prices are 0");
    }

    #[test]
    fn test_detect_best_ev_respects_threshold() {
        // Threshold of 95 means cost must be <= 95
        let config = ArbConfig::new(95, 1.0);

        // Poly YES 45 + Kalshi NO 52 = 97 + 2c fee = 99 > 95 (rejected)
        // Poly YES 43 + Kalshi NO 50 = 93 + 2c fee = 95 <= 95 (accepted)

        let kalshi = make_depth([(60, 100), (0, 0), (0, 0)], [(52, 500), (50, 1000), (0, 0)]);
        let poly = make_depth([(45, 500), (43, 1000), (0, 0)], [(60, 100), (0, 0), (0, 0)]);

        let result = detect_best_ev(1, &kalshi, &poly, &config, 12345);
        let arb = result.expect("Should find arb within threshold");

        // Level 0 (45+52+2=99) exceeds threshold 95, so rejected
        // Level 1 (43+50+2=95) meets threshold
        assert_eq!(arb.yes_price, 43);
        assert_eq!(arb.no_price, 50);
    }
}
