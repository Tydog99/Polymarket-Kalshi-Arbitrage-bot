//! High-performance order execution engine for arbitrage opportunities.
//!
//! This module handles concurrent order execution across both platforms,
//! position reconciliation, and automatic exposure management.

use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use crate::kalshi::KalshiApiClient;
use crate::poly_executor::PolyExecutor;
use crate::types::{
    ArbType, MarketPair,
    ArbOpportunity, GlobalState,
    cents_to_price,
};
use crate::circuit_breaker::CircuitBreaker;
use crate::position_tracker::{FillRecord, PositionChannel};
use crate::config::{KALSHI_WEB_BASE, build_polymarket_url};

// =============================================================================
// HELPER FUNCTIONS
// =============================================================================

/// Describe what tokens are being purchased for an arb trade
/// Returns a human-readable description like "BUY Kalshi YES + BUY Poly NO (token:abc...)"
pub fn describe_arb_trade(
    arb_type: ArbType,
    _kalshi_ticker: &str,
    poly_yes_token: &str,
    poly_no_token: &str,
    team_suffix: Option<&str>,
) -> String {
    let team_info = team_suffix.map(|t| format!(" [{}]", t)).unwrap_or_default();
    let poly_yes_short = if poly_yes_token.len() > 8 {
        format!("{}...", &poly_yes_token[..8])
    } else {
        poly_yes_token.to_string()
    };
    let poly_no_short = if poly_no_token.len() > 8 {
        format!("{}...", &poly_no_token[..8])
    } else {
        poly_no_token.to_string()
    };

    match arb_type {
        ArbType::KalshiYesPolyNo => {
            format!("BUY Kalshi YES{} + BUY Poly NO ({})", team_info, poly_no_short)
        }
        ArbType::PolyYesKalshiNo => {
            format!("BUY Poly YES ({}) + BUY Kalshi NO{}", poly_yes_short, team_info)
        }
        ArbType::PolyOnly => {
            format!("BUY Poly YES ({}) + BUY Poly NO ({})", poly_yes_short, poly_no_short)
        }
        ArbType::KalshiOnly => {
            format!("BUY Kalshi YES{0} + BUY Kalshi NO{0}", team_info)
        }
    }
}

// =============================================================================
// EXECUTION ENGINE
// =============================================================================

/// High-precision monotonic clock for latency measurement and performance tracking
pub struct NanoClock {
    start: Instant,
}

impl NanoClock {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }

    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

impl Default for NanoClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Core execution engine for processing arbitrage opportunities
pub struct ExecutionEngine {
    kalshi: Arc<KalshiApiClient>,
    poly_async: Arc<dyn PolyExecutor>,
    state: Arc<GlobalState>,
    circuit_breaker: Arc<CircuitBreaker>,
    position_channel: PositionChannel,
    in_flight: Arc<[AtomicU64; 8]>,
    clock: Arc<NanoClock>,
    pub dry_run: bool,
    test_mode: bool,
}

impl ExecutionEngine {
    pub fn new(
        kalshi: Arc<KalshiApiClient>,
        poly_async: Arc<dyn PolyExecutor>,
        state: Arc<GlobalState>,
        circuit_breaker: Arc<CircuitBreaker>,
        position_channel: PositionChannel,
        dry_run: bool,
        clock: Arc<NanoClock>,
    ) -> Self {
        let test_mode = std::env::var("TEST_ARB")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);

        Self {
            kalshi,
            poly_async,
            state,
            circuit_breaker,
            position_channel,
            in_flight: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            clock,
            dry_run,
            test_mode,
        }
    }

    /// Process an execution request
    #[inline]
    pub async fn process(&self, req: ArbOpportunity) -> Result<ExecutionResult> {
        let market_id = req.market_id;

        // Deduplication check (512 markets via 8x u64 bitmask)
        if market_id < 512 {
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            let mask = 1u64 << bit;
            let prev = self.in_flight[slot].fetch_or(mask, Ordering::AcqRel);
            if prev & mask != 0 {
                return Ok(ExecutionResult {
                    market_id,
                    success: false,
                    profit_cents: 0,
                    latency_ns: self.clock.now_ns() - req.detected_ns,
                    error: Some("Already in-flight"),
                });
            }
        }

        // Get market pair
        let market = self.state.get_by_id(market_id)
            .ok_or_else(|| anyhow!("Unknown market_id {}", market_id))?;

        let pair = market.pair()
            .ok_or_else(|| anyhow!("No pair for market_id {}", market_id))?;

        // Log detection early so we know what arb was found even if checks fail
        let profit_cents = req.profit_cents();
        let est_max_contracts = (req.yes_size.min(req.no_size) / 100) as i64;
        info!(
            "[EXEC] 📡 Detected: {} | {:?} y={}¢ n={}¢ | est_profit={}¢ | size={}¢/{}¢ | est_max={}x",
            pair.description, req.arb_type, req.yes_price, req.no_price,
            profit_cents, req.yes_size, req.no_size, est_max_contracts
        );

        // Check if league is disabled (monitor only, no execution)
        if crate::config::is_league_disabled(&pair.league) {
            info!(
                "[EXEC] 🚫 DISABLED LEAGUE: {} | {:?} y={}¢ n={}¢ | est_profit={}¢ | size={}¢/{}¢ | est_max={}x | league={}",
                pair.description, req.arb_type, req.yes_price, req.no_price,
                profit_cents, req.yes_size, req.no_size, est_max_contracts, pair.league
            );
            self.release_in_flight(market_id);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Disabled league"),
            });
        }

        // Check profit threshold
        if profit_cents < 1 {
            self.release_in_flight(market_id);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Profit below threshold"),
            });
        }

        // Calculate max contracts from size (min of both sides)
        let mut max_contracts = (req.yes_size.min(req.no_size) / 100) as i64;

        // Safety: In test mode, cap position size at 10 contracts
        // Note: Polymarket enforces a $1 minimum order value. At 40¢ per contract,
        // a single contract ($0.40) would be rejected. Using 10 contracts ensures
        // we meet the minimum requirement at any reasonable price level.
        if self.test_mode && max_contracts > 10 {
            warn!("[EXEC] ⚠️ TEST_MODE: Position size capped from {} to 10 contracts", max_contracts);
            max_contracts = 10;
        }

        if max_contracts < 1 {
            warn!(
                "[EXEC] ⚠️ Insufficient liquidity: {} | {:?} | yes={}¢ ({}¢ size) no={}¢ ({}¢ size) | need 100¢ min",
                pair.description, req.arb_type,
                req.yes_price, req.yes_size,
                req.no_price, req.no_size
            );
            // Use delayed release to avoid spam from low-liquidity markets
            self.release_in_flight_delayed(market_id);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Insufficient liquidity"),
            });
        }

        // Circuit breaker: get remaining capacity and cap if needed
        let capacity = self.circuit_breaker.get_remaining_capacity(&pair.pair_id).await;
        let min_contracts = self.circuit_breaker.min_contracts();

        // Skip trade if insufficient capacity
        if capacity.effective < min_contracts {
            warn!(
                "[EXEC] ⛔ Insufficient capacity: {} | market={} | remaining={} | min={}",
                pair.description, pair.pair_id, capacity.effective, min_contracts
            );
            self.release_in_flight(market_id);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Insufficient capacity"),
            });
        }

        // Cap contracts to remaining capacity if needed
        if max_contracts > capacity.effective {
            info!(
                "[EXEC] 📉 Capped: {} -> {} contracts | market={} | per_market={} total={}",
                max_contracts, capacity.effective, pair.description,
                capacity.per_market, capacity.total
            );
            max_contracts = capacity.effective;
        }

        // Safety net: verify with can_execute (should always pass after capping)
        if let Err(reason) = self.circuit_breaker.can_execute(&pair.pair_id, max_contracts).await {
            warn!(
                "[EXEC] ⛔ Circuit breaker blocked (post-cap): {} | market={} | contracts={}",
                reason, pair.description, max_contracts
            );
            self.release_in_flight(market_id);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Circuit breaker"),
            });
        }

        let latency_to_exec = self.clock.now_ns() - req.detected_ns;

        // Describe what tokens are being purchased
        let trade_description = describe_arb_trade(
            req.arb_type,
            &pair.kalshi_market_ticker,
            &pair.poly_yes_token,
            &pair.poly_no_token,
            pair.team_suffix.as_deref(),
        );

        info!(
            "[EXEC] 🎯 {} | {} | {:?} y={}¢ n={}¢ | profit={}¢ | {}x | {}µs",
            pair.description,
            trade_description,
            req.arb_type,
            req.yes_price,
            req.no_price,
            profit_cents,
            max_contracts,
            latency_to_exec / 1000
        );
        // Build Kalshi URL: https://kalshi.com/markets/{series}/{slug}/{event_ticker}
        let kalshi_series = pair.kalshi_event_ticker
            .split('-')
            .next()
            .unwrap_or(&pair.kalshi_event_ticker)
            .to_lowercase();
        let kalshi_event_ticker_lower = pair.kalshi_event_ticker.to_lowercase();
        let poly_url = build_polymarket_url(&pair.league, &pair.poly_slug);
        let kalshi_url = format!("{}/{}/{}/{}", KALSHI_WEB_BASE, kalshi_series, pair.kalshi_event_slug, kalshi_event_ticker_lower);
        // Log URLs (tracing routes to TUI when active)
        info!("[EXEC] 🔗 Kalshi: {} | Polymarket: {}", kalshi_url, poly_url);

        if self.dry_run {
            info!("[EXEC] 🏃 DRY RUN - would execute {} contracts", max_contracts);
            self.release_in_flight_delayed(market_id);
            return Ok(ExecutionResult {
                market_id,
                success: true,
                profit_cents,
                latency_ns: latency_to_exec,
                error: Some("DRY_RUN"),
            });
        }

        // Execute both legs concurrently
        let result = self.execute_both_legs_async(&req, &pair, max_contracts).await;

        // Release in-flight after delay
        self.release_in_flight_delayed(market_id);

        match result {
            // Note: For same-platform arbs (PolyOnly/KalshiOnly), these are YES/NO fills, not platform fills
            Ok((yes_filled, no_filled, yes_cost, no_cost, yes_order_id, no_order_id)) => {
                let matched = yes_filled.min(no_filled);
                let success = matched > 0;
                let actual_profit = matched as i16 * 100 - (yes_cost + no_cost) as i16;

                // === Automatic exposure management for mismatched fills ===
                // If one leg fills more than the other, automatically close the excess
                // to maintain market-neutral exposure (non-blocking background task)
                if yes_filled != no_filled && (yes_filled > 0 || no_filled > 0) {
                    let excess = (yes_filled - no_filled).abs();
                    let (leg1_name, leg2_name) = match req.arb_type {
                        ArbType::PolyYesKalshiNo => ("P_yes", "K_no"),
                        ArbType::KalshiYesPolyNo => ("K_yes", "P_no"),
                        ArbType::PolyOnly => ("P_yes", "P_no"),
                        ArbType::KalshiOnly => ("K_yes", "K_no"),
                    };
                    warn!("[EXEC] ⚠️ Fill mismatch: {}={} {}={} (excess={})",
                        leg1_name, yes_filled, leg2_name, no_filled, excess);

                    // Spawn auto-close in background (don't block hot path with 2s sleep)
                    let kalshi = self.kalshi.clone();
                    let poly_async = self.poly_async.clone();
                    let position_channel = self.position_channel.clone();
                    let pair_id = pair.pair_id.clone();
                    let description = pair.description.clone();
                    let arb_type = req.arb_type;
                    let yes_price = req.yes_price;
                    let no_price = req.no_price;
                    let poly_yes_token = pair.poly_yes_token.clone();
                    let poly_no_token = pair.poly_no_token.clone();
                    let kalshi_ticker = pair.kalshi_market_ticker.clone();
                    let original_cost_per_contract = if yes_filled > no_filled {
                        if yes_filled > 0 { yes_cost / yes_filled } else { 0 }
                    } else if no_filled > 0 { no_cost / no_filled } else { 0 };

                    tokio::spawn(async move {
                        Self::auto_close_background(
                            kalshi, poly_async, position_channel, pair_id, description,
                            arb_type, yes_filled, no_filled,
                            yes_price, no_price, poly_yes_token, poly_no_token,
                            kalshi_ticker, original_cost_per_contract
                        ).await;
                    });
                }

                if success {
                    self.circuit_breaker.record_success(&pair.pair_id, matched, matched, actual_profit as f64 / 100.0).await;
                }

                if matched > 0 {
                    let (platform1, side1, platform2, side2) = match req.arb_type {
                        ArbType::PolyYesKalshiNo => ("polymarket", "yes", "kalshi", "no"),
                        ArbType::KalshiYesPolyNo => ("kalshi", "yes", "polymarket", "no"),
                        ArbType::PolyOnly => ("polymarket", "yes", "polymarket", "no"),
                        ArbType::KalshiOnly => ("kalshi", "yes", "kalshi", "no"),
                    };

                    self.position_channel.record_fill(FillRecord::new(
                        &pair.pair_id, &pair.description, platform1, side1,
                        matched as f64, yes_cost as f64 / 100.0 / yes_filled.max(1) as f64,
                        0.0, &yes_order_id,
                    ));
                    self.position_channel.record_fill(FillRecord::new(
                        &pair.pair_id, &pair.description, platform2, side2,
                        matched as f64, no_cost as f64 / 100.0 / no_filled.max(1) as f64,
                        0.0, &no_order_id,
                    ));
                }

                Ok(ExecutionResult {
                    market_id,
                    success,
                    profit_cents: actual_profit,
                    latency_ns: self.clock.now_ns() - req.detected_ns,
                    error: if success { None } else { Some("Partial/no fill") },
                })
            }
            Err(_e) => {
                self.circuit_breaker.record_error().await;
                Ok(ExecutionResult {
                    market_id,
                    success: false,
                    profit_cents: 0,
                    latency_ns: self.clock.now_ns() - req.detected_ns,
                    error: Some("Execution failed"),
                })
            }
        }
    }

    async fn execute_both_legs_async(
        &self,
        req: &ArbOpportunity,
        pair: &MarketPair,
        contracts: i64,
    ) -> Result<(i64, i64, i64, i64, String, String)> {
        match req.arb_type {
            // === CROSS-PLATFORM: Poly YES + Kalshi NO ===
            ArbType::PolyYesKalshiNo => {
                let kalshi_fut = self.kalshi.buy_ioc(
                    &pair.kalshi_market_ticker,
                    "no",
                    req.no_price as i64,
                    contracts,
                );
                let poly_fut = self.poly_async.buy_fak(
                    &pair.poly_yes_token,
                    cents_to_price(req.yes_price),
                    contracts as f64,
                );
                let (kalshi_res, poly_res) = tokio::join!(kalshi_fut, poly_fut);
                // Return order: (yes_filled, no_filled, ...) = (poly, kalshi)
                self.extract_cross_results_poly_yes_kalshi_no(poly_res, kalshi_res)
            }

            // === CROSS-PLATFORM: Kalshi YES + Poly NO ===
            ArbType::KalshiYesPolyNo => {
                let kalshi_fut = self.kalshi.buy_ioc(
                    &pair.kalshi_market_ticker,
                    "yes",
                    req.yes_price as i64,
                    contracts,
                );
                let poly_fut = self.poly_async.buy_fak(
                    &pair.poly_no_token,
                    cents_to_price(req.no_price),
                    contracts as f64,
                );
                let (kalshi_res, poly_res) = tokio::join!(kalshi_fut, poly_fut);
                // Return order: (yes_filled, no_filled, ...) = (kalshi, poly)
                self.extract_cross_results_kalshi_yes_poly_no(kalshi_res, poly_res)
            }

            // === SAME-PLATFORM: Poly YES + Poly NO ===
            ArbType::PolyOnly => {
                let yes_fut = self.poly_async.buy_fak(
                    &pair.poly_yes_token,
                    cents_to_price(req.yes_price),
                    contracts as f64,
                );
                let no_fut = self.poly_async.buy_fak(
                    &pair.poly_no_token,
                    cents_to_price(req.no_price),
                    contracts as f64,
                );
                let (yes_res, no_res) = tokio::join!(yes_fut, no_fut);
                self.extract_poly_only_results(yes_res, no_res)
            }

            // === SAME-PLATFORM: Kalshi YES + Kalshi NO ===
            ArbType::KalshiOnly => {
                let yes_fut = self.kalshi.buy_ioc(
                    &pair.kalshi_market_ticker,
                    "yes",
                    req.yes_price as i64,
                    contracts,
                );
                let no_fut = self.kalshi.buy_ioc(
                    &pair.kalshi_market_ticker,
                    "no",
                    req.no_price as i64,
                    contracts,
                );
                let (yes_res, no_res) = tokio::join!(yes_fut, no_fut);
                self.extract_kalshi_only_results(yes_res, no_res)
            }
        }
    }

    /// Extract results from PolyYesKalshiNo execution.
    /// Returns: (yes_filled, no_filled, yes_cost, no_cost, yes_order_id, no_order_id)
    /// where YES = Poly and NO = Kalshi
    fn extract_cross_results_poly_yes_kalshi_no(
        &self,
        poly_res: Result<crate::polymarket_clob::PolyFillAsync>,
        kalshi_res: Result<crate::kalshi::KalshiOrderResponse>,
    ) -> Result<(i64, i64, i64, i64, String, String)> {
        let (yes_filled, yes_cost, yes_order_id) = match poly_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Poly YES failed: {}", e);
                (0, 0, String::new())
            }
        };

        let (no_filled, no_cost, no_order_id) = match kalshi_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                (filled, cost, resp.order.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Kalshi NO failed: {}", e);
                (0, 0, String::new())
            }
        };

        Ok((yes_filled, no_filled, yes_cost, no_cost, yes_order_id, no_order_id))
    }

    /// Extract results from KalshiYesPolyNo execution.
    /// Returns: (yes_filled, no_filled, yes_cost, no_cost, yes_order_id, no_order_id)
    /// where YES = Kalshi and NO = Poly
    fn extract_cross_results_kalshi_yes_poly_no(
        &self,
        kalshi_res: Result<crate::kalshi::KalshiOrderResponse>,
        poly_res: Result<crate::polymarket_clob::PolyFillAsync>,
    ) -> Result<(i64, i64, i64, i64, String, String)> {
        let (yes_filled, yes_cost, yes_order_id) = match kalshi_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                (filled, cost, resp.order.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Kalshi YES failed: {}", e);
                (0, 0, String::new())
            }
        };

        let (no_filled, no_cost, no_order_id) = match poly_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Poly NO failed: {}", e);
                (0, 0, String::new())
            }
        };

        Ok((yes_filled, no_filled, yes_cost, no_cost, yes_order_id, no_order_id))
    }

    /// Extract results from Poly-only execution (same-platform)
    fn extract_poly_only_results(
        &self,
        yes_res: Result<crate::polymarket_clob::PolyFillAsync>,
        no_res: Result<crate::polymarket_clob::PolyFillAsync>,
    ) -> Result<(i64, i64, i64, i64, String, String)> {
        let (yes_filled, yes_cost, yes_order_id) = match yes_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Poly YES failed: {}", e);
                (0, 0, String::new())
            }
        };

        let (no_filled, no_cost, no_order_id) = match no_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Poly NO failed: {}", e);
                (0, 0, String::new())
            }
        };

        // For same-platform, return YES as "kalshi" slot and NO as "poly" slot
        // This keeps the existing result handling logic working
        Ok((yes_filled, no_filled, yes_cost, no_cost, yes_order_id, no_order_id))
    }

    /// Extract results from Kalshi-only execution (same-platform)
    fn extract_kalshi_only_results(
        &self,
        yes_res: Result<crate::kalshi::KalshiOrderResponse>,
        no_res: Result<crate::kalshi::KalshiOrderResponse>,
    ) -> Result<(i64, i64, i64, i64, String, String)> {
        let (yes_filled, yes_cost, yes_order_id) = match yes_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                (filled, cost, resp.order.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Kalshi YES failed: {}", e);
                (0, 0, String::new())
            }
        };

        let (no_filled, no_cost, no_order_id) = match no_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                (filled, cost, resp.order.order_id)
            }
            Err(e) => {
                warn!("[EXEC] Kalshi NO failed: {}", e);
                (0, 0, String::new())
            }
        };

        // For same-platform, return YES as "kalshi" slot and NO as "poly" slot
        Ok((yes_filled, no_filled, yes_cost, no_cost, yes_order_id, no_order_id))
    }

    /// Background task to automatically close excess exposure from mismatched fills.
    /// Records close fills to position tracker with negative contracts.
    ///
    /// Retry strategy: Start at original_price - 1c and walk down 1c at a time
    /// until all contracts are filled or we hit the minimum price (1c).
    #[allow(clippy::too_many_arguments)]
    async fn auto_close_background(
        kalshi: Arc<KalshiApiClient>,
        poly_async: Arc<dyn PolyExecutor>,
        position_channel: PositionChannel,
        pair_id: Arc<str>,
        description: Arc<str>,
        arb_type: ArbType,
        yes_filled: i64,
        no_filled: i64,
        yes_price: u16,
        no_price: u16,
        poly_yes_token: Arc<str>,
        poly_no_token: Arc<str>,
        kalshi_ticker: Arc<str>,
        original_cost_per_contract: i64,
    ) {
        let excess = (yes_filled - no_filled).abs();
        if excess == 0 {
            return;
        }

        // Configuration for retry logic
        const MIN_PRICE_CENTS: i64 = 1;  // Don't go below 1c
        const PRICE_STEP_CENTS: i64 = 1; // Walk down 1c at a time
        const RETRY_DELAY_MS: u64 = 100; // Brief delay between retries

        // Helper to record close fill (negative contracts to reduce position)
        let record_close = |position_channel: &PositionChannel, platform: &str, side: &str,
                           closed: f64, price: f64, order_id: &str| {
            if closed > 0.0 {
                // Use negative contracts to indicate closing/selling
                position_channel.record_fill(FillRecord::new(
                    &pair_id, &description, platform, side,
                    -closed,  // Negative = closing position
                    price, 0.0, order_id,
                ));
            }
        };

        // Helper to log final P&L after close attempts
        let log_final_result = |platform: &str, total_closed: i64, total_proceeds: i64, remaining: i64| {
            if total_closed > 0 {
                let close_pnl = total_proceeds - (original_cost_per_contract * total_closed);
                info!("[EXEC] ✅ Closed {} {} contracts for {}¢ (P&L: {}¢)",
                    total_closed, platform, total_proceeds, close_pnl);
            }
            if remaining > 0 {
                error!("[EXEC] ❌ Failed to close {} {} contracts - EXPOSURE REMAINS!", remaining, platform);
            }
        };

        match arb_type {
            ArbType::PolyOnly => {
                let (token, side, start_price) = if yes_filled > no_filled {
                    (&poly_yes_token, "yes", yes_price)
                } else {
                    (&poly_no_token, "no", no_price)
                };

                info!("[EXEC] 🔄 Waiting 2s for Poly settlement before auto-close ({} {} contracts)", excess, side);
                tokio::time::sleep(Duration::from_secs(2)).await;

                Self::close_poly_with_retry(
                    &poly_async, &position_channel, token, side, start_price,
                    excess, MIN_PRICE_CENTS, PRICE_STEP_CENTS, RETRY_DELAY_MS,
                    record_close, log_final_result,
                ).await;
            }

            ArbType::KalshiOnly => {
                let (side, start_price) = if yes_filled > no_filled {
                    ("yes", yes_price as i64)
                } else {
                    ("no", no_price as i64)
                };

                Self::close_kalshi_with_retry(
                    &kalshi, &position_channel, &kalshi_ticker, side, start_price,
                    excess, MIN_PRICE_CENTS, PRICE_STEP_CENTS, RETRY_DELAY_MS,
                    record_close, log_final_result,
                ).await;
            }

            ArbType::PolyYesKalshiNo => {
                if yes_filled > no_filled {
                    // Poly YES excess - close on Poly
                    info!("[EXEC] 🔄 Waiting 2s for Poly settlement before auto-close ({} yes contracts)", excess);
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    Self::close_poly_with_retry(
                        &poly_async, &position_channel, &poly_yes_token, "yes", yes_price,
                        excess, MIN_PRICE_CENTS, PRICE_STEP_CENTS, RETRY_DELAY_MS,
                        record_close, log_final_result,
                    ).await;
                } else {
                    // Kalshi NO excess - close on Kalshi
                    Self::close_kalshi_with_retry(
                        &kalshi, &position_channel, &kalshi_ticker, "no", no_price as i64,
                        excess, MIN_PRICE_CENTS, PRICE_STEP_CENTS, RETRY_DELAY_MS,
                        record_close, log_final_result,
                    ).await;
                }
            }

            ArbType::KalshiYesPolyNo => {
                if yes_filled > no_filled {
                    // Kalshi YES excess - close on Kalshi
                    Self::close_kalshi_with_retry(
                        &kalshi, &position_channel, &kalshi_ticker, "yes", yes_price as i64,
                        excess, MIN_PRICE_CENTS, PRICE_STEP_CENTS, RETRY_DELAY_MS,
                        record_close, log_final_result,
                    ).await;
                } else {
                    // Poly NO excess - close on Poly
                    info!("[EXEC] 🔄 Waiting 2s for Poly settlement before auto-close ({} no contracts)", excess);
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    Self::close_poly_with_retry(
                        &poly_async, &position_channel, &poly_no_token, "no", no_price,
                        excess, MIN_PRICE_CENTS, PRICE_STEP_CENTS, RETRY_DELAY_MS,
                        record_close, log_final_result,
                    ).await;
                }
            }
        }
    }

    /// Close Polymarket position with retry, walking down the book 1c at a time.
    #[allow(clippy::too_many_arguments)]
    async fn close_poly_with_retry<F, G>(
        poly_async: &Arc<dyn PolyExecutor>,
        position_channel: &PositionChannel,
        token: &str,
        side: &str,
        start_price_cents: u16,
        total_to_close: i64,
        min_price_cents: i64,
        price_step_cents: i64,
        retry_delay_ms: u64,
        record_close: F,
        log_final_result: G,
    )
    where
        F: Fn(&PositionChannel, &str, &str, f64, f64, &str),
        G: Fn(&str, i64, i64, i64),
    {
        let mut remaining = total_to_close;
        let mut current_price_cents = (start_price_cents as i64).saturating_sub(1).max(min_price_cents);
        let mut total_closed: i64 = 0;
        let mut total_proceeds_cents: i64 = 0;
        let mut attempt = 0;

        while remaining > 0 && current_price_cents >= min_price_cents {
            attempt += 1;
            let price_decimal = cents_to_price(current_price_cents as u16);

            match poly_async.sell_fak(token, price_decimal, remaining as f64).await {
                Ok(fill) => {
                    let filled = fill.filled_size as i64;
                    let proceeds_cents = (fill.fill_cost * 100.0) as i64;

                    if filled > 0 {
                        info!(
                            "[EXEC] 🔄 Poly close attempt #{}: filled {}/{} @ {}c (total: {}/{})",
                            attempt, filled, remaining, current_price_cents, total_closed + filled, total_to_close
                        );

                        record_close(
                            position_channel, "polymarket", side,
                            fill.filled_size, fill.fill_cost / fill.filled_size.max(0.001), &fill.order_id
                        );

                        total_closed += filled;
                        total_proceeds_cents += proceeds_cents;
                        remaining -= filled;
                    } else {
                        info!(
                            "[EXEC] 🔄 Poly close attempt #{}: 0 filled @ {}c, stepping down",
                            attempt, current_price_cents
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "[EXEC] ⚠️ Poly close attempt #{} failed @ {}c: {}",
                        attempt, current_price_cents, e
                    );
                }
            }

            if remaining > 0 {
                current_price_cents -= price_step_cents;
                if current_price_cents >= min_price_cents {
                    info!(
                        "[EXEC] 🔄 Stepping down to {}c ({} contracts remaining)",
                        current_price_cents, remaining
                    );
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                }
            }
        }

        log_final_result("Poly", total_closed, total_proceeds_cents, remaining);
    }

    /// Close Kalshi position with retry, walking down the book 1c at a time.
    #[allow(clippy::too_many_arguments)]
    async fn close_kalshi_with_retry<F, G>(
        kalshi: &Arc<KalshiApiClient>,
        position_channel: &PositionChannel,
        ticker: &str,
        side: &str,
        start_price_cents: i64,
        total_to_close: i64,
        min_price_cents: i64,
        price_step_cents: i64,
        retry_delay_ms: u64,
        record_close: F,
        log_final_result: G,
    )
    where
        F: Fn(&PositionChannel, &str, &str, f64, f64, &str),
        G: Fn(&str, i64, i64, i64),
    {
        let mut remaining = total_to_close;
        let mut current_price_cents = start_price_cents.saturating_sub(1).max(min_price_cents);
        let mut total_closed: i64 = 0;
        let mut total_proceeds_cents: i64 = 0;
        let mut attempt = 0;

        while remaining > 0 && current_price_cents >= min_price_cents {
            attempt += 1;

            match kalshi.sell_ioc(ticker, side, current_price_cents, remaining).await {
                Ok(resp) => {
                    let filled = resp.order.filled_count();
                    let proceeds_cents = resp.order.taker_fill_cost.unwrap_or(0)
                        + resp.order.maker_fill_cost.unwrap_or(0);

                    if filled > 0 {
                        info!(
                            "[EXEC] 🔄 Kalshi close attempt #{}: filled {}/{} @ {}c (total: {}/{})",
                            attempt, filled, remaining, current_price_cents, total_closed + filled, total_to_close
                        );

                        record_close(
                            position_channel, "kalshi", side,
                            filled as f64, proceeds_cents as f64 / 100.0 / filled.max(1) as f64,
                            &resp.order.order_id
                        );

                        total_closed += filled;
                        total_proceeds_cents += proceeds_cents;
                        remaining -= filled;
                    } else {
                        info!(
                            "[EXEC] 🔄 Kalshi close attempt #{}: 0 filled @ {}c, stepping down",
                            attempt, current_price_cents
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "[EXEC] ⚠️ Kalshi close attempt #{} failed @ {}c: {}",
                        attempt, current_price_cents, e
                    );
                }
            }

            if remaining > 0 {
                current_price_cents -= price_step_cents;
                if current_price_cents >= min_price_cents {
                    info!(
                        "[EXEC] 🔄 Stepping down to {}c ({} contracts remaining)",
                        current_price_cents, remaining
                    );
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                }
            }
        }

        log_final_result("Kalshi", total_closed, total_proceeds_cents, remaining);
    }

    #[inline(always)]
    fn release_in_flight(&self, market_id: u16) {
        if market_id < 512 {
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            let mask = !(1u64 << bit);
            self.in_flight[slot].fetch_and(mask, Ordering::Release);
        }
    }

    fn release_in_flight_delayed(&self, market_id: u16) {
        if market_id < 512 {
            let in_flight = self.in_flight.clone();
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let mask = !(1u64 << bit);
                in_flight[slot].fetch_and(mask, Ordering::Release);
            });
        }
    }
}

/// Result of an execution attempt
#[derive(Debug, Clone, Copy)]
pub struct ExecutionResult {
    /// Market identifier
    pub market_id: u16,
    /// Whether execution was successful
    pub success: bool,
    /// Realized profit in cents
    pub profit_cents: i16,
    /// Total latency from detection to completion in nanoseconds
    pub latency_ns: u64,
    /// Error message if execution failed
    pub error: Option<&'static str>,
}

/// Create a new execution request channel with bounded capacity
pub fn create_execution_channel() -> (mpsc::Sender<ArbOpportunity>, mpsc::Receiver<ArbOpportunity>) {
    mpsc::channel(256)
}

/// Main execution event loop - processes arbitrage opportunities as they arrive
///
/// When `log_tx` is provided and TUI is active, logs are routed through the channel
/// instead of going to stdout (which would corrupt the TUI display).
pub async fn run_execution_loop(
    mut rx: mpsc::Receiver<ArbOpportunity>,
    engine: Arc<ExecutionEngine>,
    tui_state: Option<Arc<tokio::sync::RwLock<crate::confirm_tui::TuiState>>>,
    log_tx: Option<mpsc::Sender<String>>,
) {
    // Check TUI state and log startup message
    let tui_active = if let Some(ref state) = tui_state {
        state.read().await.active
    } else {
        false
    };

    let startup_msg = format!("[EXEC] Execution engine started (dry_run={})", engine.dry_run);
    if tui_active {
        if let Some(ref tx) = log_tx {
            let _ = tx.try_send(format!("[{}]  INFO {}", chrono::Local::now().format("%H:%M:%S"), startup_msg));
        }
    } else {
        info!("{}", startup_msg);
    }

    while let Some(req) = rx.recv().await {
        let engine = engine.clone();
        let tui_state_clone = tui_state.clone();
        let log_tx_clone = log_tx.clone();

        // Process immediately in spawned task
        tokio::spawn(async move {
            let tui_active = if let Some(ref state) = tui_state_clone {
                state.read().await.active
            } else {
                false
            };

            // Local log helpers for spawned task
            let log_info = |msg: String| {
                if tui_active {
                    if let Some(ref tx) = log_tx_clone {
                        let _ = tx.try_send(format!("[{}]  INFO {}", chrono::Local::now().format("%H:%M:%S"), msg));
                    }
                } else {
                    info!("{}", msg);
                }
            };
            let log_warn = |msg: String| {
                if tui_active {
                    if let Some(ref tx) = log_tx_clone {
                        let _ = tx.try_send(format!("[{}]  WARN {}", chrono::Local::now().format("%H:%M:%S"), msg));
                    }
                } else {
                    warn!("{}", msg);
                }
            };
            let log_error = |msg: String| {
                // Always log errors to tracing for persistence/Sentry
                error!("{}", msg);
                // Also send to TUI if active
                if tui_active {
                    if let Some(ref tx) = log_tx_clone {
                        let _ = tx.try_send(format!("[{}] ERROR {}", chrono::Local::now().format("%H:%M:%S"), msg));
                    }
                }
            };

            match engine.process(req).await {
                Ok(result) if result.success => {
                    log_info(format!(
                        "[EXEC] ✅ market_id={} profit={}¢ latency={}µs",
                        result.market_id, result.profit_cents, result.latency_ns / 1000
                    ));
                }
                Ok(result) => {
                    // Skip logging for errors that already have detailed logs
                    let already_logged = matches!(
                        result.error,
                        Some("Already in-flight") | Some("Insufficient liquidity")
                    );
                    if !already_logged {
                        let detail = engine.state.get_by_id(result.market_id)
                            .and_then(|m| m.pair())
                            .map(|p| p.description.to_string())
                            .unwrap_or_else(|| format!("market_id={}", result.market_id));
                        log_warn(format!(
                            "[EXEC] ⚠️ {}: {:?}",
                            detail, result.error
                        ));
                    }
                }
                Err(e) => {
                    log_error(format!("[EXEC] ❌ Error: {}", e));
                }
            }
        });
    }

    // Log shutdown message
    let tui_active = if let Some(ref state) = tui_state {
        state.read().await.active
    } else {
        false
    };

    let shutdown_msg = "[EXEC] Execution engine stopped";
    if tui_active {
        if let Some(ref tx) = log_tx {
            let _ = tx.try_send(format!("[{}]  INFO {}", chrono::Local::now().format("%H:%M:%S"), shutdown_msg));
        }
    } else {
        info!("{}", shutdown_msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_clock_latency_accuracy() {
        // Create a shared clock (simulating what main.rs does)
        let clock = Arc::new(NanoClock::new());

        // Simulate detection time from WebSocket handler
        let detected_ns = clock.now_ns();

        // Simulate some processing delay
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Simulate execution engine calculating latency
        let execution_ns = clock.now_ns();
        let latency_ns = execution_ns - detected_ns;

        // Latency should be approximately 5ms (5_000_000 ns), with some tolerance
        // At minimum it should be > 4ms and < 50ms
        assert!(
            latency_ns > 4_000_000,
            "Latency too low: {}ns (expected > 4ms)",
            latency_ns
        );
        assert!(
            latency_ns < 50_000_000,
            "Latency too high: {}ns (expected < 50ms)",
            latency_ns
        );
    }

    #[test]
    fn test_shared_clock_across_threads() {
        use std::sync::mpsc;

        let clock = Arc::new(NanoClock::new());

        // Channel to send detected_ns from "WebSocket thread" to "execution thread"
        let (tx, rx) = mpsc::channel();

        // Simulate WebSocket handler in another thread
        let ws_clock = clock.clone();
        let handle = std::thread::spawn(move || {
            let detected_ns = ws_clock.now_ns();
            tx.send(detected_ns).unwrap();
        });

        handle.join().unwrap();
        let detected_ns = rx.recv().unwrap();

        // Small delay
        std::thread::sleep(std::time::Duration::from_micros(100));

        // Execution thread calculates latency
        let latency_ns = clock.now_ns() - detected_ns;

        // Latency should be positive and reasonable (not billions of ns from clock mismatch)
        assert!(latency_ns > 0, "Latency should be positive");
        assert!(
            latency_ns < 100_000_000, // < 100ms
            "Latency unreasonably high: {}ns - clock sync issue?",
            latency_ns
        );
    }

    #[test]
    fn test_independent_clocks_would_fail() {
        // This test demonstrates why separate clocks are problematic
        // Two clocks created at different times have different baselines

        let clock1 = NanoClock::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let clock2 = NanoClock::new();

        // Get "detection time" from clock1
        let detected_ns = clock1.now_ns();

        // Get "execution time" from clock2 (wrong clock!)
        let execution_ns = clock2.now_ns();

        // The "latency" is nonsensical - execution_ns < detected_ns because
        // clock2 started later and has a smaller elapsed time
        // This would cause underflow or negative latency in real code
        assert!(
            execution_ns < detected_ns,
            "This test shows the bug: clock2 ({}) < clock1 ({}) due to different start times",
            execution_ns,
            detected_ns
        );
    }
}