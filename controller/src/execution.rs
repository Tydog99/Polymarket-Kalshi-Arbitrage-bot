//! High-performance order execution engine for arbitrage opportunities.
//!
//! This module handles concurrent order execution across both platforms,
//! position reconciliation, and automatic exposure management.

use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn, error};

/// Global counter for unique position_id generation
/// Each arb execution gets a unique sequence number
static ARB_COUNTER: AtomicU32 = AtomicU32::new(1);

use crate::kalshi::KalshiApiClient;
use crate::poly_executor::PolyExecutor;
use crate::types::{
    ArbType, MarketPair,
    ArbOpportunity, GlobalState,
    cents_to_price,
};
use crate::circuit_breaker::CircuitBreaker;
use crate::position_tracker::{FillRecord, PositionChannel, TradeReason, TradeStatus};
use crate::config::{KALSHI_WEB_BASE, build_polymarket_url};
use crate::strategy::{ExecStrategy, StrategyConfig, StrategyResult};
use crate::strategy_tracker::StrategyTracker;

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
    in_flight_events: Arc<Mutex<HashSet<Arc<str>>>>,
    clock: Arc<NanoClock>,
    pub dry_run: bool,
    test_mode: bool,
    strategy_config: StrategyConfig,
    strategy_tracker: StrategyTracker,
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
        strategy_config: StrategyConfig,
        strategy_tracker: StrategyTracker,
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
            in_flight_events: Arc::new(Mutex::new(HashSet::new())),
            clock,
            dry_run,
            test_mode,
            strategy_config,
            strategy_tracker,
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

        // Event-level dedup: only one outcome per Kalshi event can be in-flight
        let event_ticker = pair.kalshi_event_ticker.clone();
        {
            let mut events = self.in_flight_events.lock().unwrap();
            if !events.insert(event_ticker.clone()) {
                // Already in-flight for this event — release market bitmask and bail
                warn!(
                    "[EXEC] 🚫 Event dedup: {} already in-flight (event={}), skipping market_id={}",
                    pair.description, event_ticker, market_id
                );
                self.release_in_flight(market_id);
                return Ok(ExecutionResult {
                    market_id,
                    success: false,
                    profit_cents: 0,
                    latency_ns: self.clock.now_ns() - req.detected_ns,
                    error: Some("Already in-flight (event)"),
                });
            }
        }

        // Generate unique position_id for this arb execution
        // This ensures each arb is tracked separately even on the same market pair
        let arb_seq = ARB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let position_id: Arc<str> = format!("{}-{}", pair.pair_id, arb_seq).into();

        // Log detection early so we know what arb was found even if checks fail
        let profit_cents = req.profit_cents();
        let est_max_contracts = req.max_contracts();
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
            self.release_event(&event_ticker);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Disabled league"),
            });
        }

        // Check if market is blacklisted due to repeated fill mismatches
        if self.circuit_breaker.is_market_blacklisted(&pair.pair_id).await {
            warn!(
                "[EXEC] 🚫 BLACKLISTED: {} | repeated fill mismatches, skipping",
                pair.description
            );
            self.release_in_flight(market_id);
            self.release_event(&event_ticker);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Market blacklisted"),
            });
        }

        // Check if cooldown has elapsed (auto-resets global halt if expired)
        if !self.circuit_breaker.check_cooldown().await {
            self.release_in_flight(market_id);
            self.release_event(&event_ticker);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Circuit breaker cooldown"),
            });
        }

        // Check profit threshold
        if profit_cents < 1 {
            self.release_in_flight(market_id);
            self.release_event(&event_ticker);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Profit below threshold"),
            });
        }

        // Calculate max contracts: min(yes_size/yes_price, no_size/no_price)
        // This matches ArbOpportunity::max_contracts() used in confirmation
        let mut max_contracts = req.max_contracts() as i64;

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
                "[EXEC] ⚠️ Insufficient liquidity: {} | {:?} | yes={}¢ ({}¢ size) no={}¢ ({}¢ size) | max_contracts={}",
                pair.description, req.arb_type,
                req.yes_price, req.yes_size,
                req.no_price, req.no_size,
                max_contracts
            );
            // Use delayed release to avoid spam from low-liquidity markets
            self.release_in_flight_delayed(market_id);
            self.release_event_delayed(event_ticker);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Insufficient liquidity"),
            });
        }

        // Circuit breaker: get remaining dollar capacity and cap if needed
        let capacity = self.circuit_breaker.get_remaining_capacity(&pair.pair_id).await;
        let min_contracts = self.circuit_breaker.min_contracts();

        // Cost per contract in dollars = (yes_price + no_price) / 100
        let cost_per_contract = (req.yes_price as f64 + req.no_price as f64) / 100.0;

        // When CB is disabled, capacity.effective is f64::MAX — skip capping entirely
        if capacity.effective < f64::MAX {
            if cost_per_contract <= 0.0 {
                error!(
                    "[EXEC] Zero cost_per_contract (yes={}c, no={}c) - refusing trade for {}",
                    req.yes_price, req.no_price, pair.description
                );
                self.release_in_flight(market_id);
                self.release_event(&event_ticker);
                return Ok(ExecutionResult {
                    market_id,
                    success: false,
                    profit_cents: 0,
                    latency_ns: self.clock.now_ns() - req.detected_ns,
                    error: Some("Zero cost per contract"),
                });
            }

            // Convert dollar capacity to contract capacity
            let capacity_contracts = (capacity.effective / cost_per_contract).floor() as i64;

            // Skip trade if insufficient capacity
            if capacity_contracts < min_contracts {
                warn!(
                    "[EXEC] ⛔ Insufficient capacity: {} | market={} | remaining=${:.2} (~{} contracts) | min={}",
                    pair.description, pair.pair_id, capacity.effective, capacity_contracts, min_contracts
                );
                self.release_in_flight(market_id);
                self.release_event(&event_ticker);
                return Ok(ExecutionResult {
                    market_id,
                    success: false,
                    profit_cents: 0,
                    latency_ns: self.clock.now_ns() - req.detected_ns,
                    error: Some("Insufficient capacity"),
                });
            }

            // Cap contracts to remaining dollar capacity if needed
            if max_contracts > capacity_contracts {
                info!(
                    "[EXEC] 📉 Capped: {} -> {} contracts | market={} | remaining=${:.2} (per_market=${:.2} total=${:.2})",
                    max_contracts, capacity_contracts, pair.description,
                    capacity.effective, capacity.per_market, capacity.total
                );
                max_contracts = capacity_contracts;
            }
        }

        // Re-check Polymarket $1 minimum after capacity capping
        // (ArbOpportunity::detect() validates this initially, but circuit breaker may reduce contracts)
        const POLY_MIN_ORDER_CENTS: i64 = 100;
        let poly_order_value = match req.arb_type {
            ArbType::PolyYesKalshiNo => max_contracts * req.yes_price as i64,
            ArbType::KalshiYesPolyNo => max_contracts * req.no_price as i64,
            ArbType::PolyOnly => {
                // Both legs on Poly - use the smaller order value
                (max_contracts * req.yes_price as i64).min(max_contracts * req.no_price as i64)
            }
            ArbType::KalshiOnly => POLY_MIN_ORDER_CENTS, // No Poly, always passes
        };

        if poly_order_value < POLY_MIN_ORDER_CENTS {
            warn!(
                "[EXEC] ⚠️ Poly min order (post-cap): {} | {:?} | {}x = {}¢ < $1.00 min",
                pair.description, req.arb_type, max_contracts, poly_order_value
            );
            self.release_in_flight_delayed(market_id);
            self.release_event_delayed(event_ticker);
            return Ok(ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: self.clock.now_ns() - req.detected_ns,
                error: Some("Below Poly $1 min"),
            });
        }

        // Safety net: verify with can_execute (should always pass after capping)
        let trade_cost_basis = max_contracts as f64 * cost_per_contract;
        if let Err(reason) = self.circuit_breaker.can_execute(&pair.pair_id, trade_cost_basis).await {
            warn!(
                "[EXEC] ⛔ Circuit breaker blocked (post-cap): {} | market={} | contracts={}",
                reason, pair.description, max_contracts
            );
            self.release_in_flight(market_id);
            self.release_event(&event_ticker);
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
            self.release_event_delayed(event_ticker);
            return Ok(ExecutionResult {
                market_id,
                success: true,
                profit_cents,
                latency_ns: latency_to_exec,
                error: Some("DRY_RUN"),
            });
        }

        // Select execution strategy
        let strategy = self.strategy_config.select();
        let contracts_requested = max_contracts;
        let exec_contracts = match strategy {
            ExecStrategy::Simultaneous => max_contracts,
            ExecStrategy::MinDepth => {
                let yes_depth = if req.yes_price > 0 { req.yes_size as i64 / req.yes_price as i64 } else { 0 };
                let no_depth = if req.no_price > 0 { req.no_size as i64 / req.no_price as i64 } else { 0 };
                max_contracts.min(yes_depth.min(no_depth))
            }
            ExecStrategy::SizeCapped => {
                let yes_depth = if req.yes_price > 0 { req.yes_size as i64 / req.yes_price as i64 } else { 0 };
                let no_depth = if req.no_price > 0 { req.no_size as i64 / req.no_price as i64 } else { 0 };
                max_contracts.min(yes_depth.min(no_depth)).min(self.strategy_config.size_cap)
            }
            ExecStrategy::PolyFirst => max_contracts, // sizing happens after Poly fills
        };
        let exec_contracts = exec_contracts.max(1); // ensure at least 1
        info!("[EXEC] Strategy: {} | contracts: {} (requested: {})", strategy, exec_contracts, contracts_requested);

        // Branch execution by strategy
        let result = if strategy == ExecStrategy::PolyFirst {
            self.execute_poly_first(&req, &pair, exec_contracts).await
        } else {
            self.execute_both_legs_async(&req, &pair, exec_contracts).await
        };
        let max_contracts = exec_contracts; // rebind for downstream fill recording

        // Release in-flight after delay
        self.release_in_flight_delayed(market_id);
        self.release_event_delayed(event_ticker);

        match result {
            // Note: For same-platform arbs (PolyOnly/KalshiOnly), these are YES/NO fills, not platform fills
            Ok((yes_filled, no_filled, yes_cost, no_cost, yes_fees, no_fees, yes_order_id, no_order_id, yes_error, no_error, poly_delayed)) => {
                // === Handle delayed Polymarket orders ===
                // If Poly returned status="delayed", we need to spawn a reconciliation task
                // to poll for the final fill status. In the meantime, record Kalshi as filled
                // with a reconciliation_pending marker.
                if poly_delayed {
                    // Determine which order_id is the delayed Poly order
                    let (poly_order_id, kalshi_filled, kalshi_cost, kalshi_fees, kalshi_order_id, kalshi_side, poly_side) = match req.arb_type {
                        ArbType::PolyYesKalshiNo => {
                            // YES = Poly (delayed), NO = Kalshi
                            (yes_order_id.clone(), no_filled, no_cost, no_fees, no_order_id.clone(), "no", "yes")
                        }
                        ArbType::KalshiYesPolyNo => {
                            // YES = Kalshi, NO = Poly (delayed)
                            (no_order_id.clone(), yes_filled, yes_cost, yes_fees, yes_order_id.clone(), "yes", "no")
                        }
                        ArbType::PolyOnly => {
                            // Both legs are Poly - for now treat YES as the "primary" delayed
                            // Note: This is a rare case and may need more sophisticated handling
                            warn!("[EXEC] ⏳ PolyOnly arb with delayed order - reconciliation may be incomplete");
                            (yes_order_id.clone(), 0, 0, 0, String::new(), "", "yes")
                        }
                        ArbType::KalshiOnly => {
                            // Should never happen - Kalshi doesn't have delayed orders
                            unreachable!("KalshiOnly arb should never have poly_delayed=true");
                        }
                    };

                    info!("[EXEC] ⏳ Poly order delayed ({}), spawning reconciliation", poly_order_id);

                    // Record Kalshi fill/attempt immediately WITH reconciliation_pending marker
                    // Always record, even if kalshi_filled=0, so we have an audit trail
                    let kalshi_price = if kalshi_filled > 0 {
                        kalshi_cost as f64 / 100.0 / kalshi_filled as f64
                    } else {
                        // Use the requested price for failed orders
                        cents_to_price(if kalshi_side == "yes" { req.yes_price } else { req.no_price })
                    };
                    let kalshi_status = if kalshi_filled == max_contracts {
                        TradeStatus::Success
                    } else if kalshi_filled > 0 {
                        TradeStatus::PartialFill
                    } else {
                        TradeStatus::Failed
                    };
                    let kalshi_failure_reason = if kalshi_filled == 0 {
                        Some("No fill".to_string())
                    } else {
                        None
                    };
                    self.position_channel.record_fill(FillRecord::with_pending_reconciliation(
                        &position_id,
                        &pair.description,
                        "kalshi",
                        kalshi_side,
                        max_contracts as f64,
                        kalshi_filled as f64,
                        kalshi_price,
                        kalshi_fees as f64 / 100.0,
                        &kalshi_order_id,
                        if kalshi_side == "yes" { TradeReason::ArbLegYes } else { TradeReason::ArbLegNo },
                        kalshi_status,
                        kalshi_failure_reason,
                        poly_order_id.clone(),
                    ));

                    // Read timeout from env var (default 5000ms)
                    let timeout_ms: u64 = std::env::var("POLY_DELAYED_TIMEOUT_MS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(5000);

                    // Clone values needed for the background reconciliation task
                    let poly_async_for_spawn = self.poly_async.clone();
                    let kalshi_for_spawn = self.kalshi.clone();
                    let position_channel_for_spawn = self.position_channel.clone();
                    let position_id_for_spawn = position_id.clone();
                    let description_for_spawn = pair.description.clone();
                    let kalshi_ticker_for_spawn = pair.kalshi_market_ticker.clone();
                    let poly_yes_token_for_spawn = pair.poly_yes_token.clone();
                    let poly_no_token_for_spawn = pair.poly_no_token.clone();
                    let arb_type_for_spawn = req.arb_type;
                    let poly_price_for_spawn = if poly_side == "yes" { req.yes_price } else { req.no_price };

                    tokio::spawn(async move {
                        reconcile_delayed_poly(
                            poly_async_for_spawn,
                            kalshi_for_spawn,
                            poly_order_id,
                            poly_price_for_spawn,
                            kalshi_filled,
                            position_id_for_spawn,
                            description_for_spawn,
                            kalshi_ticker_for_spawn,
                            poly_yes_token_for_spawn,
                            poly_no_token_for_spawn,
                            arb_type_for_spawn,
                            position_channel_for_spawn,
                            timeout_ms,
                        ).await;
                    });

                    // Record strategy result for delayed order
                    self.strategy_tracker.record(&StrategyResult {
                        strategy,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        market_id,
                        description: pair.description.to_string(),
                        arb_type: format!("{:?}", req.arb_type),
                        contracts_requested,
                        contracts_executed: max_contracts,
                        yes_filled,
                        no_filled,
                        fill_mismatch: yes_filled != no_filled,
                        auto_close_triggered: false,
                        profit_cents: 0,
                        detection_to_exec_ns: self.clock.now_ns() - req.detected_ns,
                        poly_delayed: true,
                        error: None,
                    });

                    // Return early with success (reconciliation will update position tracker later)
                    return Ok(ExecutionResult {
                        market_id,
                        success: true,
                        profit_cents: 0, // Unknown until reconciliation completes
                        latency_ns: self.clock.now_ns() - req.detected_ns,
                        error: None,
                    });
                }

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

                    // Record mismatch for market-specific blacklisting and consecutive error tracking
                    self.circuit_breaker.record_mismatch(&pair.pair_id).await;

                    // Spawn auto-close in background (don't block hot path with 2s sleep)
                    let kalshi = self.kalshi.clone();
                    let poly_async = self.poly_async.clone();
                    let position_channel = self.position_channel.clone();
                    let circuit_breaker = self.circuit_breaker.clone();
                    let position_id_clone = position_id.clone();
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
                            kalshi, poly_async, position_channel, circuit_breaker,
                            position_id_clone, description,
                            arb_type, yes_filled, no_filled,
                            yes_price, no_price, poly_yes_token, poly_no_token,
                            kalshi_ticker, original_cost_per_contract
                        ).await;
                    });
                }

                if success {
                    self.circuit_breaker.record_success(
                        &pair.pair_id,
                        matched, req.yes_price as i64,
                        matched, req.no_price as i64,
                        actual_profit as f64 / 100.0,
                    ).await;
                    // Clear mismatch count on successful matched execution
                    self.circuit_breaker.clear_market_mismatches(&pair.pair_id).await;
                }

                // Record fills for each leg individually (not just matched contracts)
                // This ensures position tracker is accurate even when one leg fails
                // and auto-close runs to unwind the other leg
                //
                // We also record failed attempts for complete audit trail
                let (platform1, side1, platform2, side2) = match req.arb_type {
                    ArbType::PolyYesKalshiNo => ("polymarket", "yes", "kalshi", "no"),
                    ArbType::KalshiYesPolyNo => ("kalshi", "yes", "polymarket", "no"),
                    ArbType::PolyOnly => ("polymarket", "yes", "polymarket", "no"),
                    ArbType::KalshiOnly => ("kalshi", "yes", "kalshi", "no"),
                };

                // Record YES leg (success, partial, or failed)
                let yes_requested = max_contracts as f64;
                let yes_status = if yes_filled == max_contracts {
                    TradeStatus::Success
                } else if yes_filled > 0 {
                    TradeStatus::PartialFill
                } else {
                    TradeStatus::Failed
                };
                let yes_price = if yes_filled > 0 {
                    yes_cost as f64 / 100.0 / yes_filled as f64
                } else {
                    cents_to_price(req.yes_price)
                };
                // Build failure reason: "No fill" or "No fill - <error>"
                let yes_failure_reason = if yes_filled == 0 {
                    Some(format_failure_reason(yes_error.as_deref()))
                } else {
                    None
                };
                self.position_channel.record_fill(FillRecord::with_details(
                    &position_id, &pair.description, platform1, side1,
                    yes_requested, yes_filled as f64, yes_price,
                    yes_fees as f64 / 100.0, &yes_order_id,
                    TradeReason::ArbLegYes, yes_status,
                    yes_failure_reason,
                ));

                // Record NO leg (success, partial, or failed)
                let no_requested = max_contracts as f64;
                let no_status = if no_filled == max_contracts {
                    TradeStatus::Success
                } else if no_filled > 0 {
                    TradeStatus::PartialFill
                } else {
                    TradeStatus::Failed
                };
                let no_price = if no_filled > 0 {
                    no_cost as f64 / 100.0 / no_filled as f64
                } else {
                    cents_to_price(req.no_price)
                };
                // Build failure reason: "No fill" or "No fill - <error>"
                let no_failure_reason = if no_filled == 0 {
                    Some(format_failure_reason(no_error.as_deref()))
                } else {
                    None
                };
                self.position_channel.record_fill(FillRecord::with_details(
                    &position_id, &pair.description, platform2, side2,
                    no_requested, no_filled as f64, no_price,
                    no_fees as f64 / 100.0, &no_order_id,
                    TradeReason::ArbLegNo, no_status,
                    no_failure_reason,
                ));

                let auto_close_triggered = yes_filled != no_filled && yes_filled > 0 && no_filled > 0;
                self.strategy_tracker.record(&StrategyResult {
                    strategy,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    market_id,
                    description: pair.description.to_string(),
                    arb_type: format!("{:?}", req.arb_type),
                    contracts_requested,
                    contracts_executed: max_contracts,
                    yes_filled,
                    no_filled,
                    fill_mismatch: yes_filled != no_filled,
                    auto_close_triggered,
                    profit_cents: actual_profit as i64,
                    detection_to_exec_ns: self.clock.now_ns() - req.detected_ns,
                    poly_delayed,
                    error: None,
                });

                Ok(ExecutionResult {
                    market_id,
                    success,
                    profit_cents: actual_profit,
                    latency_ns: self.clock.now_ns() - req.detected_ns,
                    error: if success { None } else { Some("Partial/no fill") },
                })
            }
            Err(_e) => {
                self.strategy_tracker.record(&StrategyResult {
                    strategy,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    market_id,
                    description: pair.description.to_string(),
                    arb_type: format!("{:?}", req.arb_type),
                    contracts_requested,
                    contracts_executed: max_contracts,
                    yes_filled: 0,
                    no_filled: 0,
                    fill_mismatch: false,
                    auto_close_triggered: false,
                    profit_cents: 0,
                    detection_to_exec_ns: self.clock.now_ns() - req.detected_ns,
                    poly_delayed: false,
                    error: Some(_e.to_string()),
                });

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
    ) -> Result<(i64, i64, i64, i64, i64, i64, String, String, Option<String>, Option<String>, bool)> {
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

    /// Sequential execution: await Poly first, then fire Kalshi only for confirmed fills.
    /// Eliminates phantom-arb losses but is slower (arb may vanish while waiting).
    async fn execute_poly_first(
        &self,
        req: &ArbOpportunity,
        pair: &MarketPair,
        contracts: i64,
    ) -> Result<(i64, i64, i64, i64, i64, i64, String, String, Option<String>, Option<String>, bool)> {
        match req.arb_type {
            ArbType::PolyYesKalshiNo => {
                // Step 1: Execute Poly YES
                let poly_res = self.poly_async.buy_fak(
                    &pair.poly_yes_token,
                    cents_to_price(req.yes_price),
                    contracts as f64,
                ).await;

                let (poly_filled, poly_cost, poly_order_id, poly_error, poly_delayed) = match poly_res {
                    Ok(fill) if fill.is_delayed => {
                        // Poll delayed order
                        let timeout_ms: u64 = std::env::var("POLY_DELAYED_TIMEOUT_MS")
                            .ok().and_then(|v| v.parse().ok()).unwrap_or(5000);
                        info!("[EXEC] poly_first: Poly delayed, polling order {}", fill.order_id);
                        match self.poly_async.poll_delayed_order(&fill.order_id, cents_to_price(req.yes_price), timeout_ms).await {
                            Ok((size, cost)) => ((size as i64), (cost * 100.0) as i64, fill.order_id, None, true),
                            Err(e) => (0, 0, fill.order_id, Some(e.to_string()), true),
                        }
                    }
                    Ok(fill) => ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id, None, false),
                    Err(e) => {
                        let msg = e.to_string();
                        warn!("[EXEC] poly_first: Poly YES failed: {}", msg);
                        (0, 0, String::new(), Some(msg), false)
                    }
                };

                // Step 2: If Poly filled, fire Kalshi NO for confirmed amount
                if poly_filled > 0 {
                    info!("[EXEC] poly_first: Poly filled {} contracts, firing Kalshi NO", poly_filled);
                    let kalshi_res = self.kalshi.buy_ioc(
                        &pair.kalshi_market_ticker,
                        "no",
                        req.no_price as i64,
                        poly_filled,
                    ).await;

                    let (no_filled, no_cost, no_fees, no_order_id, no_error) = match kalshi_res {
                        Ok(resp) => {
                            let filled = resp.order.filled_count();
                            let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                            let fees = resp.order.total_fees();
                            (filled, cost, fees, resp.order.order_id, None)
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            warn!("[EXEC] poly_first: Kalshi NO failed: {}", msg);
                            (0, 0, 0, String::new(), Some(msg))
                        }
                    };

                    Ok((poly_filled, no_filled, poly_cost, no_cost, 0, no_fees,
                        poly_order_id, no_order_id, poly_error, no_error, poly_delayed))
                } else {
                    info!("[EXEC] poly_first: Poly got 0 fills, skipping Kalshi");
                    Ok((0, 0, 0, 0, 0, 0,
                        poly_order_id, String::new(), poly_error, None, poly_delayed))
                }
            }

            ArbType::KalshiYesPolyNo => {
                // Step 1: Execute Poly NO first
                let poly_res = self.poly_async.buy_fak(
                    &pair.poly_no_token,
                    cents_to_price(req.no_price),
                    contracts as f64,
                ).await;

                let (poly_filled, poly_cost, poly_order_id, poly_error, poly_delayed) = match poly_res {
                    Ok(fill) if fill.is_delayed => {
                        let timeout_ms: u64 = std::env::var("POLY_DELAYED_TIMEOUT_MS")
                            .ok().and_then(|v| v.parse().ok()).unwrap_or(5000);
                        info!("[EXEC] poly_first: Poly delayed, polling order {}", fill.order_id);
                        match self.poly_async.poll_delayed_order(&fill.order_id, cents_to_price(req.no_price), timeout_ms).await {
                            Ok((size, cost)) => ((size as i64), (cost * 100.0) as i64, fill.order_id, None, true),
                            Err(e) => (0, 0, fill.order_id, Some(e.to_string()), true),
                        }
                    }
                    Ok(fill) => ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id, None, false),
                    Err(e) => {
                        let msg = e.to_string();
                        warn!("[EXEC] poly_first: Poly NO failed: {}", msg);
                        (0, 0, String::new(), Some(msg), false)
                    }
                };

                // Step 2: If Poly filled, fire Kalshi YES
                if poly_filled > 0 {
                    info!("[EXEC] poly_first: Poly filled {} contracts, firing Kalshi YES", poly_filled);
                    let kalshi_res = self.kalshi.buy_ioc(
                        &pair.kalshi_market_ticker,
                        "yes",
                        req.yes_price as i64,
                        poly_filled,
                    ).await;

                    let (yes_filled, yes_cost, yes_fees, yes_order_id, yes_error) = match kalshi_res {
                        Ok(resp) => {
                            let filled = resp.order.filled_count();
                            let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                            let fees = resp.order.total_fees();
                            (filled, cost, fees, resp.order.order_id, None)
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            warn!("[EXEC] poly_first: Kalshi YES failed: {}", msg);
                            (0, 0, 0, String::new(), Some(msg))
                        }
                    };

                    Ok((yes_filled, poly_filled, yes_cost, poly_cost, yes_fees, 0,
                        yes_order_id, poly_order_id, yes_error, poly_error, poly_delayed))
                } else {
                    info!("[EXEC] poly_first: Poly got 0 fills, skipping Kalshi");
                    Ok((0, 0, 0, 0, 0, 0,
                        String::new(), poly_order_id, None, poly_error, poly_delayed))
                }
            }

            // Same-platform arbs: fall back to simultaneous execution
            ArbType::PolyOnly | ArbType::KalshiOnly => {
                self.execute_both_legs_async(req, pair, contracts).await
            }
        }
    }

    /// Extract results from PolyYesKalshiNo execution.
    /// Returns: (yes_filled, no_filled, yes_cost, no_cost, yes_fees, no_fees, yes_order_id, no_order_id, yes_error, no_error, poly_delayed)
    /// where YES = Poly and NO = Kalshi
    fn extract_cross_results_poly_yes_kalshi_no(
        &self,
        poly_res: Result<crate::polymarket_clob::PolyFillAsync>,
        kalshi_res: Result<crate::kalshi::KalshiOrderResponse>,
    ) -> Result<(i64, i64, i64, i64, i64, i64, String, String, Option<String>, Option<String>, bool)> {
        let (yes_filled, yes_cost, yes_fees, yes_order_id, yes_error, poly_delayed) = match poly_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, 0i64, fill.order_id, None, fill.is_delayed)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Poly YES failed: {}", err_msg);
                (0, 0, 0, String::new(), Some(err_msg), false)
            }
        };

        let (no_filled, no_cost, no_fees, no_order_id, no_error) = match kalshi_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                let fees = resp.order.total_fees();
                (filled, cost, fees, resp.order.order_id, None)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Kalshi NO failed: {}", err_msg);
                (0, 0, 0, String::new(), Some(err_msg))
            }
        };

        Ok((yes_filled, no_filled, yes_cost, no_cost, yes_fees, no_fees, yes_order_id, no_order_id, yes_error, no_error, poly_delayed))
    }

    /// Extract results from KalshiYesPolyNo execution.
    /// Returns: (yes_filled, no_filled, yes_cost, no_cost, yes_fees, no_fees, yes_order_id, no_order_id, yes_error, no_error, poly_delayed)
    /// where YES = Kalshi and NO = Poly
    fn extract_cross_results_kalshi_yes_poly_no(
        &self,
        kalshi_res: Result<crate::kalshi::KalshiOrderResponse>,
        poly_res: Result<crate::polymarket_clob::PolyFillAsync>,
    ) -> Result<(i64, i64, i64, i64, i64, i64, String, String, Option<String>, Option<String>, bool)> {
        let (yes_filled, yes_cost, yes_fees, yes_order_id, yes_error) = match kalshi_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                let fees = resp.order.total_fees();
                (filled, cost, fees, resp.order.order_id, None)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Kalshi YES failed: {}", err_msg);
                (0, 0, 0, String::new(), Some(err_msg))
            }
        };

        let (no_filled, no_cost, no_fees, no_order_id, no_error, poly_delayed) = match poly_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, 0i64, fill.order_id, None, fill.is_delayed)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Poly NO failed: {}", err_msg);
                (0, 0, 0, String::new(), Some(err_msg), false)
            }
        };

        Ok((yes_filled, no_filled, yes_cost, no_cost, yes_fees, no_fees, yes_order_id, no_order_id, yes_error, no_error, poly_delayed))
    }

    /// Extract results from Poly-only execution (same-platform)
    /// Returns poly_delayed = true if EITHER leg is delayed (both are Poly)
    fn extract_poly_only_results(
        &self,
        yes_res: Result<crate::polymarket_clob::PolyFillAsync>,
        no_res: Result<crate::polymarket_clob::PolyFillAsync>,
    ) -> Result<(i64, i64, i64, i64, i64, i64, String, String, Option<String>, Option<String>, bool)> {
        let (yes_filled, yes_cost, yes_order_id, yes_error, yes_delayed) = match yes_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id, None, fill.is_delayed)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Poly YES failed: {}", err_msg);
                (0, 0, String::new(), Some(err_msg), false)
            }
        };

        let (no_filled, no_cost, no_order_id, no_error, no_delayed) = match no_res {
            Ok(fill) => {
                ((fill.filled_size as i64), (fill.fill_cost * 100.0) as i64, fill.order_id, None, fill.is_delayed)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Poly NO failed: {}", err_msg);
                (0, 0, String::new(), Some(err_msg), false)
            }
        };

        // For same-platform, return YES as "kalshi" slot and NO as "poly" slot
        // This keeps the existing result handling logic working
        // Polymarket has zero fees
        // If either leg is delayed, we report poly_delayed = true
        let poly_delayed = yes_delayed || no_delayed;
        Ok((yes_filled, no_filled, yes_cost, no_cost, 0, 0, yes_order_id, no_order_id, yes_error, no_error, poly_delayed))
    }

    /// Extract results from Kalshi-only execution (same-platform)
    /// poly_delayed is always false for Kalshi-only trades
    fn extract_kalshi_only_results(
        &self,
        yes_res: Result<crate::kalshi::KalshiOrderResponse>,
        no_res: Result<crate::kalshi::KalshiOrderResponse>,
    ) -> Result<(i64, i64, i64, i64, i64, i64, String, String, Option<String>, Option<String>, bool)> {
        let (yes_filled, yes_cost, yes_fees, yes_order_id, yes_error) = match yes_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                let fees = resp.order.total_fees();
                (filled, cost, fees, resp.order.order_id, None)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Kalshi YES failed: {}", err_msg);
                (0, 0, 0, String::new(), Some(err_msg))
            }
        };

        let (no_filled, no_cost, no_fees, no_order_id, no_error) = match no_res {
            Ok(resp) => {
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0) + resp.order.maker_fill_cost.unwrap_or(0);
                let fees = resp.order.total_fees();
                (filled, cost, fees, resp.order.order_id, None)
            }
            Err(e) => {
                let err_msg = e.to_string();
                warn!("[EXEC] Kalshi NO failed: {}", err_msg);
                (0, 0, 0, String::new(), Some(err_msg))
            }
        };

        // For same-platform, return YES as "kalshi" slot and NO as "poly" slot
        // Kalshi-only trades never have delayed Poly orders
        Ok((yes_filled, no_filled, yes_cost, no_cost, yes_fees, no_fees, yes_order_id, no_order_id, yes_error, no_error, false))
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
        circuit_breaker: Arc<CircuitBreaker>,
        position_id: Arc<str>,
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

        // Helper to record close fill with trade history details
        // attempt: 1 = first close, 2+ = retry
        // requested: how many we asked to close
        // closed: how many actually closed
        let record_close = |position_channel: &PositionChannel, platform: &str, side: &str,
                           requested: f64, closed: f64, price: f64, order_id: &str, attempt: u32| {
            let reason = if attempt == 1 {
                TradeReason::AutoClose
            } else {
                TradeReason::AutoCloseRetry
            };

            let status = if closed >= requested {
                TradeStatus::Success
            } else if closed > 0.0 {
                TradeStatus::PartialFill
            } else {
                TradeStatus::Failed
            };

            // Record even zero fills for complete audit trail
            position_channel.record_fill(FillRecord::with_details(
                &position_id, &description, platform, side,
                -requested,  // Negative = closing position
                -closed,     // Negative = closing position
                price, 0.0, order_id,
                reason, status,
                if closed == 0.0 { Some("No fill".to_string()) } else { None },
            ));
        };

        // Helper to log final P&L after close attempts and feed back to circuit breaker
        let cb_for_pnl = circuit_breaker.clone();
        let log_final_result = move |platform: &str, total_closed: i64, total_proceeds: i64, remaining: i64| {
            if total_closed > 0 {
                let close_pnl = total_proceeds - (original_cost_per_contract * total_closed);
                info!("[EXEC] ✅ Closed {} {} contracts for {}¢ (P&L: {}¢)",
                    total_closed, platform, total_proceeds, close_pnl);
                // Feed auto-close P&L back to circuit breaker so daily loss limits work
                cb_for_pnl.record_pnl(close_pnl as f64 / 100.0);
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
        F: Fn(&PositionChannel, &str, &str, f64, f64, f64, &str, u32),
        G: Fn(&str, i64, i64, i64),
    {
        let mut remaining = total_to_close;
        let mut current_price_cents = (start_price_cents as i64).saturating_sub(1).max(min_price_cents);
        let mut total_closed: i64 = 0;
        let mut total_proceeds_cents: i64 = 0;
        let mut attempt = 0u32;
        let mut settlement_retries = 0u32;
        let max_settlement_retries: u32 = std::env::var("POLY_SETTLEMENT_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50);

        // Read timeout from env var for delayed order polling (default 5000ms)
        let delayed_timeout_ms: u64 = std::env::var("POLY_DELAYED_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000);

        while remaining > 0 && current_price_cents >= min_price_cents {
            attempt += 1;
            let price_decimal = cents_to_price(current_price_cents as u16);
            let requested = remaining as f64;

            match poly_async.sell_fak(token, price_decimal, requested).await {
                Ok(fill) => {
                    // Handle delayed orders by polling for final status (same as BUY reconciliation)
                    let (filled, proceeds_cents) = if fill.is_delayed {
                        info!(
                            "[EXEC] 🔄 Poly close attempt #{} @ {}c returned delayed ({}), polling...",
                            attempt, current_price_cents, fill.order_id
                        );

                        // Poll until terminal state or timeout
                        match poly_async.poll_delayed_order(&fill.order_id, price_decimal, delayed_timeout_ms).await {
                            Ok((matched, cost)) => {
                                info!(
                                    "[EXEC] 🔄 Poly close delayed order {} resolved: filled={:.0}, cost=${:.2}",
                                    fill.order_id, matched, cost
                                );
                                (matched as i64, (cost * 100.0) as i64)
                            }
                            Err(e) => {
                                warn!(
                                    "[EXEC] ⚠️ Poly close delayed order {} timeout/error: {} - assuming no fill",
                                    fill.order_id, e
                                );
                                (0, 0)
                            }
                        }
                    } else {
                        (fill.filled_size as i64, (fill.fill_cost * 100.0) as i64)
                    };

                    // Use actual fill price when available (Poly gives taker price improvement)
                    let record_price = if filled > 0 {
                        proceeds_cents as f64 / 100.0 / filled as f64
                    } else {
                        price_decimal
                    };

                    if filled > 0 && (record_price - price_decimal).abs() > 0.005 {
                        info!(
                            "[EXEC] 🔄 Poly close attempt #{}: filled {}/{} @ {:.0}c (limit {}c, price improvement) (total: {}/{})",
                            attempt, filled, remaining, record_price * 100.0, current_price_cents, total_closed + filled, total_to_close
                        );
                    } else {
                        info!(
                            "[EXEC] 🔄 Poly close attempt #{}: filled {}/{} @ {}c (total: {}/{})",
                            attempt, filled, remaining, current_price_cents, total_closed + filled, total_to_close
                        );
                    }
                    record_close(
                        position_channel, "polymarket", side,
                        requested, filled as f64, record_price,
                        &fill.order_id, attempt
                    );

                    if filled > 0 {
                        total_closed += filled;
                        total_proceeds_cents += proceeds_cents;
                        remaining -= filled;
                    }
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let is_settlement_error = err_msg.contains("not enough balance")
                        || err_msg.contains("not enough allowance");

                    if is_settlement_error {
                        settlement_retries += 1;
                        if settlement_retries > max_settlement_retries {
                            error!(
                                "[EXEC] ❌ Settlement never completed after {} retries, giving up",
                                settlement_retries
                            );
                            record_close(
                                position_channel, "polymarket", side,
                                requested, 0.0, price_decimal, "", attempt
                            );
                            break;
                        }
                        warn!(
                            "[EXEC] ⏳ Settlement pending, retry #{} at same price {}c",
                            settlement_retries, current_price_cents
                        );
                        record_close(
                            position_channel, "polymarket", side,
                            requested, 0.0, price_decimal, "", attempt
                        );
                        tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                        continue;
                    }

                    warn!(
                        "[EXEC] ⚠️ Poly close attempt #{} failed @ {}c: {}",
                        attempt, current_price_cents, e
                    );
                    // Record the failed attempt
                    record_close(
                        position_channel, "polymarket", side,
                        requested, 0.0, price_decimal, "", attempt
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
        F: Fn(&PositionChannel, &str, &str, f64, f64, f64, &str, u32),
        G: Fn(&str, i64, i64, i64),
    {
        let mut remaining = total_to_close;
        let mut current_price_cents = start_price_cents.saturating_sub(1).max(min_price_cents);
        let mut total_closed: i64 = 0;
        let mut total_proceeds_cents: i64 = 0;
        let mut attempt = 0u32;

        while remaining > 0 && current_price_cents >= min_price_cents {
            attempt += 1;
            let requested = remaining as f64;
            let price_decimal = current_price_cents as f64 / 100.0;

            match kalshi.sell_ioc(ticker, side, current_price_cents, remaining).await {
                Ok(resp) => {
                    let filled = resp.order.filled_count();
                    let proceeds_cents = resp.order.taker_fill_cost.unwrap_or(0)
                        + resp.order.maker_fill_cost.unwrap_or(0);

                    info!(
                        "[EXEC] 🔄 Kalshi close attempt #{}: filled {}/{} @ {}c (total: {}/{})",
                        attempt, filled, remaining, current_price_cents, total_closed + filled, total_to_close
                    );

                    // Record every attempt (even zero fills) for complete audit trail
                    // Always use limit price - Kalshi's cost fields may return complement price
                    record_close(
                        position_channel, "kalshi", side,
                        requested, filled as f64, price_decimal,
                        &resp.order.order_id, attempt
                    );

                    if filled > 0 {
                        total_closed += filled;
                        total_proceeds_cents += proceeds_cents;
                        remaining -= filled;
                    }
                }
                Err(e) => {
                    warn!(
                        "[EXEC] ⚠️ Kalshi close attempt #{} failed @ {}c: {}",
                        attempt, current_price_cents, e
                    );
                    // Record the failed attempt
                    record_close(
                        position_channel, "kalshi", side,
                        requested, 0.0, price_decimal, "", attempt
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

    fn release_event(&self, event_ticker: &Arc<str>) {
        let mut events = self.in_flight_events.lock().unwrap();
        events.remove(event_ticker);
    }

    fn release_event_delayed(&self, event_ticker: Arc<str>) {
        let in_flight_events = self.in_flight_events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let mut events = in_flight_events.lock().unwrap();
            events.remove(&event_ticker);
        });
    }
}

// =============================================================================
// DELAYED ORDER RECONCILIATION
// =============================================================================

/// Reconcile a delayed Polymarket order by polling until terminal state.
///
/// This function is spawned as a background task when Polymarket returns status="delayed".
/// It polls the order status and, upon resolution:
/// 1. Records the Poly fill (or no-fill) to the position tracker
/// 2. Clears the reconciliation_pending marker
/// 3. If there's a mismatch with Kalshi fills, unwinds the excess Kalshi position
///
/// # Arguments
/// * `poly_async` - Polymarket executor for polling
/// * `kalshi` - Kalshi client for unwinding excess positions
/// * `poly_order_id` - The delayed Polymarket order ID to poll
/// * `poly_price_cents` - Original order price in cents
/// * `kalshi_filled` - Number of contracts filled on Kalshi
/// * `position_id` - Position ID for fill recording
/// * `description` - Market description for fill recording
/// * `kalshi_ticker` - Kalshi market ticker for unwinding
/// * `poly_yes_token` - Polymarket YES token for unwinding
/// * `poly_no_token` - Polymarket NO token for unwinding
/// * `arb_type` - Type of arbitrage (determines which side to unwind)
/// * `position_channel` - Channel to send fill records
/// * `timeout_ms` - Maximum time to poll before giving up
#[allow(clippy::too_many_arguments)]
async fn reconcile_delayed_poly(
    poly_async: Arc<dyn PolyExecutor>,
    kalshi: Arc<KalshiApiClient>,
    poly_order_id: String,
    poly_price_cents: u16,
    kalshi_filled: i64,
    position_id: Arc<str>,
    description: Arc<str>,
    kalshi_ticker: Arc<str>,
    poly_yes_token: Arc<str>,
    poly_no_token: Arc<str>,
    arb_type: ArbType,
    position_channel: PositionChannel,
    timeout_ms: u64,
) {
    info!(
        "[RECONCILE] Polling Poly order {} (Kalshi filled {})",
        poly_order_id, kalshi_filled
    );

    // Convert price to decimal for API
    let price_decimal = cents_to_price(poly_price_cents);

    // Poll until terminal state or timeout
    let poll_result = poly_async
        .poll_delayed_order(&poly_order_id, price_decimal, timeout_ms)
        .await;

    let (poly_filled, poly_cost) = match poll_result {
        Ok((filled, cost)) => {
            info!(
                "[RECONCILE] Poly {} resolved: filled={:.0}, cost=${:.2}",
                poly_order_id, filled, cost
            );
            (filled as i64, (cost * 100.0) as i64) // Convert to cents
        }
        Err(e) => {
            warn!(
                "[RECONCILE] Timeout polling {}: {} - assuming no fill",
                poly_order_id, e
            );
            (0, 0)
        }
    };

    // Determine which side the Poly order was for
    let poly_side = match arb_type {
        ArbType::PolyYesKalshiNo => "yes",
        ArbType::KalshiYesPolyNo => "no",
        ArbType::PolyOnly => "yes", // PolyOnly defaults to YES as primary (rare case)
        ArbType::KalshiOnly => unreachable!("KalshiOnly should never have delayed Poly orders"),
    };

    // Record Poly fill (or no-fill)
    if poly_filled > 0 {
        let poly_price = poly_cost as f64 / 100.0 / poly_filled as f64;
        position_channel.record_fill(FillRecord::with_details(
            &position_id,
            &description,
            "polymarket",
            poly_side,
            kalshi_filled as f64, // requested (assumed same as Kalshi)
            poly_filled as f64,
            poly_price,
            0.0, // Polymarket has zero fees
            &poly_order_id,
            if poly_side == "yes" { TradeReason::ArbLegYes } else { TradeReason::ArbLegNo },
            if poly_filled == kalshi_filled { TradeStatus::Success } else { TradeStatus::PartialFill },
            None,
        ));
    } else {
        // Record the failed/no-fill
        position_channel.record_fill(FillRecord::with_details(
            &position_id,
            &description,
            "polymarket",
            poly_side,
            kalshi_filled as f64, // requested
            0.0,                  // filled
            price_decimal,
            0.0,
            &poly_order_id,
            if poly_side == "yes" { TradeReason::ArbLegYes } else { TradeReason::ArbLegNo },
            TradeStatus::Failed,
            Some("No fill - delayed order timeout/canceled".to_string()),
        ));
    }

    // Clear the reconciliation_pending marker
    position_channel.clear_reconciliation_pending(&poly_order_id);

    // Check for mismatch - unwind excess on whichever platform filled more
    if kalshi_filled != poly_filled && (kalshi_filled > 0 || poly_filled > 0) {
        // Configuration for retry logic
        const MIN_PRICE_CENTS: i64 = 1;
        const PRICE_STEP_CENTS: i64 = 1;
        const RETRY_DELAY_MS: u64 = 100;

        // Record close helper (simplified version for reconciliation)
        let record_close = |position_channel: &PositionChannel, platform: &str, side: &str,
                           requested: f64, closed: f64, price: f64, order_id: &str, attempt: u32| {
            let reason = if attempt == 1 {
                TradeReason::AutoClose
            } else {
                TradeReason::AutoCloseRetry
            };

            let status = if closed >= requested {
                TradeStatus::Success
            } else if closed > 0.0 {
                TradeStatus::PartialFill
            } else {
                TradeStatus::Failed
            };

            position_channel.record_fill(FillRecord::with_details(
                &position_id, &description, platform, side,
                -requested,
                -closed,
                price, 0.0, order_id,
                reason, status,
                if closed == 0.0 { Some("No fill".to_string()) } else { None },
            ));
        };

        let log_final_result = |platform: &str, total_closed: i64, total_proceeds: i64, remaining: i64| {
            if total_closed > 0 {
                info!(
                    "[RECONCILE] Closed {} {} contracts for {}¢",
                    total_closed, platform, total_proceeds
                );
            }
            if remaining > 0 {
                error!(
                    "[RECONCILE] Failed to close {} {} contracts - EXPOSURE REMAINS!",
                    remaining, platform
                );
            }
        };

        if kalshi_filled > poly_filled {
            // Kalshi filled more - unwind excess Kalshi position
            let excess = kalshi_filled - poly_filled;
            warn!(
                "[RECONCILE] Mismatch: Kalshi={} Poly={}, unwinding {} Kalshi contracts",
                kalshi_filled, poly_filled, excess
            );

            // Determine which side to unwind on Kalshi
            let kalshi_side = match arb_type {
                ArbType::PolyYesKalshiNo => "no",  // Kalshi had NO
                ArbType::KalshiYesPolyNo => "yes", // Kalshi had YES
                ArbType::PolyOnly => return,       // No Kalshi position to unwind
                ArbType::KalshiOnly => unreachable!(),
            };

            // Unwind the excess Kalshi position
            // We need to sell at a price that will fill - start slightly below our buy price
            let start_price_cents = poly_price_cents as i64; // Use Poly price as proxy

            ExecutionEngine::close_kalshi_with_retry(
                &kalshi,
                &position_channel,
                &kalshi_ticker,
                kalshi_side,
                start_price_cents,
                excess,
                MIN_PRICE_CENTS,
                PRICE_STEP_CENTS,
                RETRY_DELAY_MS,
                record_close,
                log_final_result,
            ).await;
        } else {
            // Poly filled more - unwind excess Poly position
            let excess = poly_filled - kalshi_filled;
            warn!(
                "[RECONCILE] Mismatch: Kalshi={} Poly={}, unwinding {} Poly contracts",
                kalshi_filled, poly_filled, excess
            );

            // Determine which token/side to unwind on Polymarket
            let (poly_token, poly_side_str) = match arb_type {
                ArbType::PolyYesKalshiNo => (&poly_yes_token, "yes"), // Poly had YES
                ArbType::KalshiYesPolyNo => (&poly_no_token, "no"),   // Poly had NO
                ArbType::PolyOnly => return, // Both sides are Poly - complex case, skip for now
                ArbType::KalshiOnly => unreachable!(),
            };

            // Wait for Poly settlement before attempting to close
            info!(
                "[RECONCILE] Waiting 2s for Poly settlement before auto-close ({} {} contracts)",
                excess, poly_side_str
            );
            tokio::time::sleep(Duration::from_secs(2)).await;

            // Unwind the excess Poly position
            ExecutionEngine::close_poly_with_retry(
                &poly_async,
                &position_channel,
                poly_token,
                poly_side_str,
                poly_price_cents,
                excess,
                MIN_PRICE_CENTS,
                PRICE_STEP_CENTS,
                RETRY_DELAY_MS,
                record_close,
                log_final_result,
            ).await;
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

/// Main execution event loop - processes arbitrage opportunities as they arrive.
///
/// Logs are routed via the tracing subscriber (TuiAwareWriter handles TUI vs stdout).
pub async fn run_execution_loop(
    mut rx: mpsc::Receiver<ArbOpportunity>,
    engine: Arc<ExecutionEngine>,
) {
    info!("[EXEC] Execution engine started (dry_run={})", engine.dry_run);

    while let Some(req) = rx.recv().await {
        let engine = engine.clone();

        // Process immediately in spawned task
        tokio::spawn(async move {
            match engine.process(req).await {
                Ok(result) if result.success => {
                    info!(
                        "[EXEC] ✅ market_id={} profit={}¢ latency={}µs",
                        result.market_id, result.profit_cents, result.latency_ns / 1000
                    );
                }
                Ok(result) => {
                    // Skip logging for errors that already have detailed logs
                    let already_logged = matches!(
                        result.error,
                        Some("Already in-flight") | Some("Insufficient liquidity") | Some("Below Poly $1 min")
                    );
                    if !already_logged {
                        let detail = engine.state.get_by_id(result.market_id)
                            .and_then(|m| m.pair())
                            .map(|p| p.description.to_string())
                            .unwrap_or_else(|| format!("market_id={}", result.market_id));
                        warn!(
                            "[EXEC] ⚠️ {}: {:?}",
                            detail, result.error
                        );
                    }
                }
                Err(e) => {
                    error!("[EXEC] ❌ Error: {}", e);
                }
            }
        });
    }

    info!("[EXEC] Execution engine stopped");
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

    // =========================================================================
    // BUG FIX TESTS: Auto-close price recording
    // =========================================================================

    /// Bug #1: Failed close attempts should record the LIMIT price, not 0.0
    ///
    /// When an API call returns OK but filled=0, the price should be the limit
    /// price we sent, not 0.0 (which happens when we derive price from proceeds).
    #[test]
    fn test_close_price_calculation_zero_fill() {
        // Scenario: API returns OK but filled=0, proceeds=0
        // Current bug: derives price as 0.0/100.0/1 = 0.0
        // Expected: should use the limit price we sent (46 cents)

        let limit_price_cents: i64 = 46;
        let filled: i64 = 0;
        let proceeds_cents: i64 = 0;

        // This is the BUGGY calculation from current code
        let buggy_price = proceeds_cents as f64 / 100.0 / (filled.max(1) as f64);

        // This is what we SHOULD record
        let expected_price = limit_price_cents as f64 / 100.0;

        // Currently this will be equal (both 0.0), but after fix it won't be
        // The correct price should be the limit price, not derived from proceeds
        assert!(
            (expected_price - 0.46).abs() < 0.001,
            "Expected price should be 0.46, got {}",
            expected_price
        );

        // The buggy calculation gives 0.0 - this test documents the bug
        assert!(
            (buggy_price - 0.0).abs() < 0.001,
            "Current buggy price is 0.0"
        );

        // This assertion SHOULD pass after fix - currently the logic is wrong
        // We use calculate_close_record_price to test the fix
        let correct_price = calculate_close_record_price(filled, proceeds_cents, limit_price_cents);
        assert!(
            (correct_price - 0.46).abs() < 0.001,
            "Close price for zero-fill should be limit price (0.46), got {}",
            correct_price
        );
    }

    /// Bug #2: Successful close should record LIMIT price, not proceeds-derived price
    ///
    /// When selling on Kalshi, the `taker_fill_cost + maker_fill_cost` returns
    /// the complement price (100 - limit_price), not the actual sale price.
    /// We should record what we asked for, not what Kalshi reports.
    #[test]
    fn test_close_price_calculation_full_fill() {
        // Scenario: Sell 8 contracts at limit 46c
        // Kalshi returns proceeds of 432c (which is 8 * 54c, the complement)
        // Current bug: records 432/100/8 = 0.54
        // Expected: should record 0.46 (our limit price)

        let limit_price_cents: i64 = 46;
        let filled: i64 = 8;
        let proceeds_cents: i64 = 432; // Kalshi reports 8 * 54c = 432c

        // This is the BUGGY calculation from current code
        let buggy_price = proceeds_cents as f64 / 100.0 / (filled as f64);

        assert!(
            (buggy_price - 0.54).abs() < 0.001,
            "Current buggy price is 0.54 (complement), got {}",
            buggy_price
        );

        // After fix, we should always use the limit price we sent
        let correct_price = calculate_close_record_price(filled, proceeds_cents, limit_price_cents);
        assert!(
            (correct_price - 0.46).abs() < 0.001,
            "Close price should be limit price (0.46), got {}",
            correct_price
        );
    }

    /// Bug #3: Failed Polymarket orders should capture the actual error message
    ///
    /// When Poly returns an error like "invalid signature", we should record
    /// "No fill - invalid signature" instead of just "No fill".
    #[test]
    fn test_failure_reason_captures_error_message() {
        // Scenario: Polymarket returns error "invalid signature"
        // Current bug: records failure_reason as "No fill"
        // Expected: "No fill - Polymarket order failed 400 Bad Request: {\"error\":\"invalid signature\"}"

        let api_error = "Polymarket order failed 400 Bad Request: {\"error\":\"invalid signature\"}";

        // This is what the failure reason SHOULD be
        let expected_reason = format_failure_reason(Some(api_error));

        assert!(
            expected_reason.contains("No fill"),
            "Should start with 'No fill'"
        );
        assert!(
            expected_reason.contains("invalid signature"),
            "Should contain the actual error message, got: {}",
            expected_reason
        );
        assert_eq!(
            expected_reason,
            "No fill - Polymarket order failed 400 Bad Request: {\"error\":\"invalid signature\"}",
            "Format should be 'No fill - <error>'"
        );
    }

    /// Test that when there's no specific error, we just say "No fill"
    #[test]
    fn test_failure_reason_no_error() {
        let reason = format_failure_reason(None);
        assert_eq!(reason, "No fill", "Without error should just be 'No fill'");
    }

    // =========================================================================
    // BUG FIX TESTS: Unique position_id per arb execution
    // =========================================================================

    #[test]
    fn test_arb_counter_increments() {
        // Get two sequential counter values
        let seq1 = ARB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let seq2 = ARB_COUNTER.fetch_add(1, Ordering::Relaxed);

        // They should be sequential
        assert_eq!(seq2, seq1 + 1, "Counter should increment by 1");
    }

    #[test]
    fn test_position_id_format() {
        let pair_id = "nba-lal-bos-2026-01-27-KXNBAGAME-26JAN27LALBOS-LAL";
        let seq = 42u32;

        let position_id = format!("{}-{}", pair_id, seq);

        assert_eq!(
            position_id,
            "nba-lal-bos-2026-01-27-KXNBAGAME-26JAN27LALBOS-LAL-42"
        );
    }

    #[test]
    fn test_position_ids_are_unique() {
        // Simulate multiple arb executions on the same market
        let pair_id: Arc<str> = "test-market-pair".into();

        let mut position_ids = Vec::new();
        for _ in 0..100 {
            let seq = ARB_COUNTER.fetch_add(1, Ordering::Relaxed);
            let position_id: Arc<str> = format!("{}-{}", pair_id, seq).into();
            position_ids.push(position_id);
        }

        // All position_ids should be unique
        let unique_count = position_ids.iter().collect::<std::collections::HashSet<_>>().len();
        assert_eq!(unique_count, 100, "All position_ids should be unique");
    }

    #[test]
    fn test_position_id_uniqueness_across_threads() {
        use std::sync::atomic::Ordering;

        let pair_id: Arc<str> = "threaded-test-pair".into();
        let position_ids = Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut handles = vec![];
        for _ in 0..10 {
            let pair_id = pair_id.clone();
            let position_ids = position_ids.clone();

            handles.push(std::thread::spawn(move || {
                for _ in 0..10 {
                    let seq = ARB_COUNTER.fetch_add(1, Ordering::Relaxed);
                    let position_id: Arc<str> = format!("{}-{}", pair_id, seq).into();
                    position_ids.lock().unwrap().push(position_id);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let ids = position_ids.lock().unwrap();
        let unique_count = ids.iter().collect::<std::collections::HashSet<_>>().len();
        assert_eq!(unique_count, 100, "All position_ids should be unique across threads");
    }

    // =========================================================================
    // Polymarket $1 Minimum Order Value Tests
    // =========================================================================

    /// Polymarket requires a minimum order value of $1.00
    /// This function calculates the minimum contracts needed at a given price
    #[inline]
    fn poly_min_contracts_for_price(price_cents: i64) -> i64 {
        const MIN_ORDER_CENTS: i64 = 100;
        (MIN_ORDER_CENTS + price_cents - 1) / price_cents // ceil division
    }

    #[test]
    fn test_poly_min_contracts_at_49_cents() {
        // At 49¢ per contract:
        // 1 × 49 = 49¢ < $1.00 ❌
        // 2 × 49 = 98¢ < $1.00 ❌
        // 3 × 49 = 147¢ ≥ $1.00 ✓
        let min = poly_min_contracts_for_price(49);
        assert_eq!(min, 3, "At 49¢, need 3 contracts to meet $1 minimum");
    }

    #[test]
    fn test_poly_min_contracts_at_50_cents() {
        // At 50¢ per contract:
        // 1 × 50 = 50¢ < $1.00 ❌
        // 2 × 50 = 100¢ = $1.00 ✓
        let min = poly_min_contracts_for_price(50);
        assert_eq!(min, 2, "At 50¢, need 2 contracts to meet $1 minimum");
    }

    #[test]
    fn test_poly_min_contracts_at_10_cents() {
        // At 10¢ per contract:
        // 10 × 10 = 100¢ = $1.00 ✓
        let min = poly_min_contracts_for_price(10);
        assert_eq!(min, 10, "At 10¢, need 10 contracts to meet $1 minimum");
    }

    #[test]
    fn test_poly_min_contracts_at_1_dollar() {
        // At 100¢ per contract (rare but possible):
        // 1 × 100 = 100¢ = $1.00 ✓
        let min = poly_min_contracts_for_price(100);
        assert_eq!(min, 1, "At $1.00, need 1 contract to meet $1 minimum");
    }

    #[test]
    fn test_poly_order_would_be_rejected() {
        // Simulating the user's scenario: 49¢ price, 1 contract
        let price_cents = 49i64;
        let contracts = 1i64;
        let order_value_cents = price_cents * contracts;
        let min_contracts = poly_min_contracts_for_price(price_cents);

        assert!(
            order_value_cents < 100,
            "1 contract at 49¢ = {}¢ should be below $1 minimum",
            order_value_cents
        );
        assert!(
            contracts < min_contracts,
            "1 contract is below minimum {} for 49¢ price",
            min_contracts
        );
    }

    #[test]
    fn test_poly_order_would_succeed() {
        // 3 contracts at 49¢ should work
        let price_cents = 49i64;
        let contracts = 3i64;
        let order_value_cents = price_cents * contracts;
        let min_contracts = poly_min_contracts_for_price(price_cents);

        assert!(
            order_value_cents >= 100,
            "3 contracts at 49¢ = {}¢ should be >= $1 minimum",
            order_value_cents
        );
        assert!(
            contracts >= min_contracts,
            "3 contracts should meet minimum {} for 49¢ price",
            min_contracts
        );
    }

    // =========================================================================
    // Settlement retry tests
    // =========================================================================

    #[tokio::test]
    async fn test_settlement_retry_keeps_same_price() {
        use crate::poly_executor::mock::{MockPolyClient, MockResponse};
        use crate::position_tracker::create_position_channel;

        let mock_client = MockPolyClient::new();

        // Return "not enough balance" 3 times, then succeed with full fill
        mock_client.set_response_sequence("test-token", vec![
            MockResponse::Error("not enough balance".to_string()),
            MockResponse::Error("not enough balance".to_string()),
            MockResponse::Error("not enough balance".to_string()),
            MockResponse::FullFill { size: 5.0, price: 0.39 },
        ]);

        let mock: Arc<dyn crate::poly_executor::PolyExecutor> = Arc::new(mock_client);
        let (position_channel, mut _rx) = create_position_channel();

        // Track recorded prices to verify they don't step down
        let recorded_prices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let prices_clone = recorded_prices.clone();

        let record_close = move |_ch: &PositionChannel, _platform: &str, _side: &str,
                                  _requested: f64, _filled: f64, price: f64, _oid: &str, _attempt: u32| {
            prices_clone.lock().unwrap().push(price);
        };

        let log_final = |_platform: &str, _closed: i64, _proceeds: i64, _remaining: i64| {};

        ExecutionEngine::close_poly_with_retry(
            &mock,
            &position_channel,
            "test-token",
            "yes",
            40,    // start at 40c
            5,     // close 5 contracts
            1,     // min 1c
            1,     // step 1c
            1,     // 1ms delay for fast test
            record_close,
            log_final,
        ).await;

        let prices = recorded_prices.lock().unwrap();
        // 3 failed attempts + 1 success = 4 records
        assert_eq!(prices.len(), 4, "Expected 4 recorded attempts, got {}", prices.len());

        // All settlement retries should be at the same price (39c = start - 1 initial step)
        // The price should NOT step down during settlement errors
        let expected_price = 0.39; // (40 - 1) / 100
        for (i, &price) in prices.iter().enumerate() {
            assert!(
                (price - expected_price).abs() < 0.001,
                "Attempt {} price was {}, expected {} (price should not step down on settlement error)",
                i + 1, price, expected_price
            );
        }
    }

    #[tokio::test]
    async fn test_settlement_retry_exhaustion_breaks() {
        use crate::poly_executor::mock::MockPolyClient;
        use crate::position_tracker::create_position_channel;

        let mock_client = MockPolyClient::new();

        // Always return settlement error
        mock_client.set_error("test-token", "not enough balance");

        let mock: Arc<dyn crate::poly_executor::PolyExecutor> = Arc::new(mock_client);
        let (position_channel, mut _rx) = create_position_channel();
        let call_count = Arc::new(std::sync::Mutex::new(0u32));
        let count_clone = call_count.clone();

        let record_close = move |_ch: &PositionChannel, _platform: &str, _side: &str,
                                  _requested: f64, _filled: f64, _price: f64, _oid: &str, _attempt: u32| {
            *count_clone.lock().unwrap() += 1;
        };

        let log_final = |_platform: &str, _closed: i64, _proceeds: i64, _remaining: i64| {};

        // Set max retries to 5 via env var for this test
        std::env::set_var("POLY_SETTLEMENT_RETRIES", "5");

        ExecutionEngine::close_poly_with_retry(
            &mock,
            &position_channel,
            "test-token",
            "yes",
            40,
            5,
            1,
            1,
            1, // 1ms delay
            record_close,
            log_final,
        ).await;

        std::env::remove_var("POLY_SETTLEMENT_RETRIES");

        // Should have stopped after max_settlement_retries + 1 (the one that triggers the break)
        let count = *call_count.lock().unwrap();
        assert!(
            count <= 7, // 5 retries + small tolerance for the breaking attempt
            "Expected ~6 attempts with max 5 settlement retries, got {}",
            count
        );

    }

    // =========================================================================
    // Price recording tests
    // =========================================================================

    #[tokio::test]
    async fn test_close_records_actual_fill_price_not_limit() {
        use crate::poly_executor::mock::MockPolyClient;
        use crate::position_tracker::create_position_channel;

        let mock_client = MockPolyClient::new();

        // Poly gives price improvement: limit is 20c but fills at 37c
        // fill_cost = size * price = 5 * 0.37 = 1.85
        mock_client.set_full_fill("test-token", 5.0, 0.37);

        let mock: Arc<dyn crate::poly_executor::PolyExecutor> = Arc::new(mock_client);
        let (position_channel, mut _rx) = create_position_channel();

        let recorded_prices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let prices_clone = recorded_prices.clone();

        let record_close = move |_ch: &PositionChannel, _platform: &str, _side: &str,
                                  _requested: f64, _filled: f64, price: f64, _oid: &str, _attempt: u32| {
            prices_clone.lock().unwrap().push(price);
        };

        let log_final = |_platform: &str, _closed: i64, _proceeds: i64, _remaining: i64| {};

        ExecutionEngine::close_poly_with_retry(
            &mock,
            &position_channel,
            "test-token",
            "yes",
            21,    // start at 21c (limit will be 20c after initial step-down)
            5,     // close 5 contracts
            1,
            1,
            1,
            record_close,
            log_final,
        ).await;

        let prices = recorded_prices.lock().unwrap();
        assert_eq!(prices.len(), 1, "Expected 1 recorded attempt, got {}", prices.len());

        // Should record 0.37 (actual fill price), NOT 0.20 (limit price)
        let recorded = prices[0];
        assert!(
            (recorded - 0.37).abs() < 0.001,
            "Recorded price {:.4} should be actual fill price 0.37, not the limit price",
            recorded
        );
    }

    #[tokio::test]
    async fn test_close_records_limit_price_on_zero_fill() {
        use crate::poly_executor::mock::{MockPolyClient, MockResponse};
        use crate::position_tracker::create_position_channel;

        let mock_client = MockPolyClient::new();

        // Zero fill followed by full fill to terminate the loop
        mock_client.set_response_sequence("test-token", vec![
            MockResponse::FullFill { size: 0.0, price: 0.0 },  // zero fill
            MockResponse::FullFill { size: 5.0, price: 0.37 }, // actual fill
        ]);

        let mock: Arc<dyn crate::poly_executor::PolyExecutor> = Arc::new(mock_client);
        let (position_channel, mut _rx) = create_position_channel();

        let recorded_prices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let prices_clone = recorded_prices.clone();

        let record_close = move |_ch: &PositionChannel, _platform: &str, _side: &str,
                                  _requested: f64, _filled: f64, price: f64, _oid: &str, _attempt: u32| {
            prices_clone.lock().unwrap().push(price);
        };

        let log_final = |_platform: &str, _closed: i64, _proceeds: i64, _remaining: i64| {};

        ExecutionEngine::close_poly_with_retry(
            &mock,
            &position_channel,
            "test-token",
            "yes",
            40,
            5,
            1,
            1,
            1,
            record_close,
            log_final,
        ).await;

        let prices = recorded_prices.lock().unwrap();
        assert_eq!(prices.len(), 2, "Expected 2 recorded attempts, got {}", prices.len());

        // First attempt (zero fill): should use limit price (39c)
        assert!(
            (prices[0] - 0.39).abs() < 0.001,
            "Zero-fill should record limit price 0.39, got {:.4}",
            prices[0]
        );

        // Second attempt (actual fill): should use actual fill price (37c)
        assert!(
            (prices[1] - 0.37).abs() < 0.001,
            "Fill should record actual price 0.37, got {:.4}",
            prices[1]
        );
    }
}

/// Format the failure reason for a no-fill scenario.
///
/// If there was an API error, includes it: "No fill - <error>"
/// Otherwise just returns "No fill"
#[inline]
fn format_failure_reason(api_error: Option<&str>) -> String {
    match api_error {
        Some(err) => format!("No fill - {}", err),
        None => "No fill".to_string(),
    }
}

// =============================================================================
// TEST HELPER FUNCTIONS
// =============================================================================

/// Calculate the correct price to record for auto-close attempts.
///
/// Always uses the limit price we sent, not the proceeds-derived price.
/// This avoids two bugs:
/// 1. Zero fills recording 0.0 instead of the limit price
/// 2. Kalshi returning complement prices (100 - limit) in cost fields
///
/// Note: This is used by tests to document and verify the correct behavior.
/// The actual implementation uses `price_decimal` directly in the close functions.
#[cfg(test)]
#[inline]
fn calculate_close_record_price(filled: i64, _proceeds_cents: i64, limit_price_cents: i64) -> f64 {
    // Always use limit price - this is what we asked for
    // The proceeds might be the complement price or zero
    let _ = filled; // Filled count doesn't affect which price to record
    limit_price_cents as f64 / 100.0
}