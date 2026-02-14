//! Risk management and circuit breaker system.
//!
//! This module provides configurable risk limits, position tracking, and
//! automatic trading halt mechanisms to protect against excessive losses.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, warn, info};

/// Circuit breaker configuration from environment
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Maximum dollar exposure per market (cost basis in dollars)
    pub max_position_per_market: f64,

    /// Maximum total dollar exposure across all markets (cost basis in dollars)
    pub max_total_position: f64,

    /// Maximum daily loss (in dollars) before halting
    pub max_daily_loss: f64,

    /// Maximum number of consecutive errors before halting
    pub max_consecutive_errors: u32,

    /// Cooldown period after a trip (seconds)
    pub cooldown_secs: u64,

    /// Whether circuit breakers are enabled
    pub enabled: bool,

    /// Minimum contracts to execute (trades below this are skipped)
    pub min_contracts: i64,
}

/// Parse a positive f64 from an env var, logging warnings for invalid values.
fn parse_positive_f64_env(name: &str, default: f64) -> f64 {
    match std::env::var(name) {
        Ok(v) => match v.parse::<f64>() {
            Ok(val) if val.is_finite() && val > 0.0 => val,
            Ok(val) => {
                warn!("[CB] {}={} is invalid (must be finite and > 0), using default ${:.2}", name, val, default);
                default
            }
            Err(_) => {
                warn!("[CB] {}='{}' is not a valid number, using default ${:.2}", name, v, default);
                default
            }
        },
        Err(_) => default,
    }
}

impl CircuitBreakerConfig {
    pub fn from_env() -> Self {
        Self {
            max_position_per_market: parse_positive_f64_env("CB_MAX_POSITION_PER_MARKET", 500.0),
            max_total_position: parse_positive_f64_env("CB_MAX_TOTAL_POSITION", 1000.0),
            max_daily_loss: parse_positive_f64_env("CB_MAX_DAILY_LOSS", 500.0),

            max_consecutive_errors: std::env::var("CB_MAX_CONSECUTIVE_ERRORS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),

            cooldown_secs: std::env::var("CB_COOLDOWN_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300), // 5 minutes default

            enabled: std::env::var("CB_ENABLED")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(true), // Enabled by default for safety

            min_contracts: std::env::var("CB_MIN_CONTRACTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
        }
    }
}

/// Reason why circuit breaker was tripped
#[derive(Debug, Clone, PartialEq)]
pub enum TripReason {
    MaxPositionPerMarket { market: String, position: f64, limit: f64 },
    MaxTotalPosition { position: f64, limit: f64 },
    MaxDailyLoss { loss: f64, limit: f64 },
    ConsecutiveErrors { count: u32, limit: u32 },
    ManualHalt,
}

impl std::fmt::Display for TripReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TripReason::MaxPositionPerMarket { market, position, limit } => {
                write!(f, "Max position per market: {} at ${:.2} (limit: ${:.2})", market, position, limit)
            }
            TripReason::MaxTotalPosition { position, limit } => {
                write!(f, "Max total position: ${:.2} (limit: ${:.2})", position, limit)
            }
            TripReason::MaxDailyLoss { loss, limit } => {
                write!(f, "Max daily loss: ${:.2} (limit: ${:.2})", loss, limit)
            }
            TripReason::ConsecutiveErrors { count, limit } => {
                write!(f, "Consecutive errors: {} (limit: {})", count, limit)
            }
            TripReason::ManualHalt => {
                write!(f, "Manual halt triggered")
            }
        }
    }
}

/// Remaining capacity for trading, used for capping trade sizes.
/// All values are in dollars (cost basis).
#[derive(Debug, Clone, Copy)]
pub struct RemainingCapacity {
    /// Remaining dollar capacity for this specific market
    pub per_market: f64,
    /// Remaining dollar capacity across all markets
    pub total: f64,
    /// Effective remaining dollar capacity (minimum of per_market and total)
    pub effective: f64,
}

/// Position tracking for a single market.
/// Tracks total dollar cost basis (contracts × price) across all legs.
#[derive(Debug, Default)]
pub struct MarketPosition {
    pub total_cost: f64,
}

impl MarketPosition {
    pub fn total_cost_basis(&self) -> f64 {
        self.total_cost
    }
}

/// Per-market failure tracking for blacklisting markets with repeated failures
#[derive(Debug)]
struct MarketFailureState {
    /// Number of consecutive mismatch failures on this market
    consecutive_mismatches: u32,
    /// When this market was blacklisted (None = not blacklisted)
    blacklisted_at: Option<Instant>,
}

/// Default: 3 consecutive mismatches before blacklisting a market
const DEFAULT_MARKET_BLACKLIST_THRESHOLD: u32 = 3;
/// Default: 5 minutes blacklist duration per market
const DEFAULT_MARKET_BLACKLIST_SECS: u64 = 300;

/// Circuit breaker state
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,

    /// Whether trading is currently halted
    halted: AtomicBool,

    /// When the circuit breaker was tripped
    tripped_at: RwLock<Option<Instant>>,

    /// Reason for trip
    trip_reason: RwLock<Option<TripReason>>,

    /// Consecutive error count
    consecutive_errors: AtomicI64,

    /// Daily P&L tracking (in cents)
    daily_pnl_cents: AtomicI64,

    /// Positions per market
    positions: RwLock<std::collections::HashMap<String, MarketPosition>>,

    /// Per-market failure tracking for blacklisting
    market_failures: RwLock<std::collections::HashMap<String, MarketFailureState>>,

    /// Threshold for blacklisting a market (consecutive mismatches)
    market_blacklist_threshold: u32,

    /// How long a market stays blacklisted (seconds)
    market_blacklist_secs: u64,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        info!("[CB] Circuit breaker initialized:");
        info!("[CB]   Enabled: {}", config.enabled);
        info!("[CB]   Max position per market: ${:.2}", config.max_position_per_market);
        info!("[CB]   Max total position: ${:.2}", config.max_total_position);
        info!("[CB]   Max daily loss: ${:.2}", config.max_daily_loss);
        info!("[CB]   Max consecutive errors: {}", config.max_consecutive_errors);
        info!("[CB]   Cooldown: {}s", config.cooldown_secs);
        info!("[CB]   Min contracts: {}", config.min_contracts);
        
        let market_blacklist_threshold: u32 = std::env::var("CB_MARKET_BLACKLIST_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MARKET_BLACKLIST_THRESHOLD);
        let market_blacklist_secs: u64 = std::env::var("CB_MARKET_BLACKLIST_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MARKET_BLACKLIST_SECS);
        info!("[CB]   Market blacklist threshold: {} mismatches", market_blacklist_threshold);
        info!("[CB]   Market blacklist duration: {}s", market_blacklist_secs);

        Self {
            config,
            halted: AtomicBool::new(false),
            tripped_at: RwLock::new(None),
            trip_reason: RwLock::new(None),
            consecutive_errors: AtomicI64::new(0),
            daily_pnl_cents: AtomicI64::new(0),
            positions: RwLock::new(std::collections::HashMap::new()),
            market_failures: RwLock::new(std::collections::HashMap::new()),
            market_blacklist_threshold,
            market_blacklist_secs,
        }
    }
    
    /// Check if trading is allowed
    #[allow(dead_code)]
    pub fn is_trading_allowed(&self) -> bool {
        if !self.config.enabled {
            return true;
        }
        !self.halted.load(Ordering::SeqCst)
    }

    /// Get the minimum contracts threshold
    pub fn min_contracts(&self) -> i64 {
        self.config.min_contracts
    }

    /// Get remaining dollar capacity for a specific market.
    /// Returns how many more dollars of exposure can be added for this market.
    pub async fn get_remaining_capacity(&self, market_id: &str) -> RemainingCapacity {
        if !self.config.enabled {
            return RemainingCapacity {
                per_market: f64::MAX,
                total: f64::MAX,
                effective: f64::MAX,
            };
        }

        let positions = self.positions.read().await;

        // Calculate remaining per-market dollar capacity
        let current_market_cost = positions
            .get(market_id)
            .map(|p| p.total_cost_basis())
            .unwrap_or(0.0);
        let per_market = self.config.max_position_per_market - current_market_cost;

        // Calculate remaining total dollar capacity
        let total_cost: f64 = positions.values().map(|p| p.total_cost_basis()).sum();
        let total = self.config.max_total_position - total_cost;

        // Effective is the minimum of both
        let effective = per_market.min(total).max(0.0);

        RemainingCapacity {
            per_market: per_market.max(0.0),
            total: total.max(0.0),
            effective,
        }
    }

    /// Check if we can execute a trade for a specific market.
    /// `cost_basis` is the total dollar cost of the proposed trade (contracts × price / 100).
    pub async fn can_execute(&self, market_id: &str, cost_basis: f64) -> Result<(), TripReason> {
        if !self.config.enabled {
            return Ok(());
        }

        if self.halted.load(Ordering::SeqCst) {
            let reason = self.trip_reason.read().await;
            return Err(reason.clone().unwrap_or(TripReason::ManualHalt));
        }

        // Check position limits (in dollars)
        let positions = self.positions.read().await;

        // Per-market limit
        let current_market_cost = positions
            .get(market_id)
            .map(|p| p.total_cost_basis())
            .unwrap_or(0.0);
        let new_cost = current_market_cost + cost_basis;
        if new_cost > self.config.max_position_per_market {
            return Err(TripReason::MaxPositionPerMarket {
                market: market_id.to_string(),
                position: new_cost,
                limit: self.config.max_position_per_market,
            });
        }

        // Total position limit
        let total_cost: f64 = positions.values().map(|p| p.total_cost_basis()).sum();
        if total_cost + cost_basis > self.config.max_total_position {
            return Err(TripReason::MaxTotalPosition {
                position: total_cost + cost_basis,
                limit: self.config.max_total_position,
            });
        }

        // Daily loss limit
        let daily_loss = -self.daily_pnl_cents.load(Ordering::SeqCst) as f64 / 100.0;
        if daily_loss > self.config.max_daily_loss {
            return Err(TripReason::MaxDailyLoss {
                loss: daily_loss,
                limit: self.config.max_daily_loss,
            });
        }

        Ok(())
    }

    /// Record a successful execution.
    /// Prices are in cents. Cost basis is computed as `contracts × price_cents / 100.0`.
    pub async fn record_success(
        &self,
        market_id: &str,
        kalshi_contracts: i64,
        kalshi_price_cents: i64,
        poly_contracts: i64,
        poly_price_cents: i64,
        pnl: f64,
    ) {
        // Reset consecutive errors
        self.consecutive_errors.store(0, Ordering::SeqCst);

        // Update P&L
        let pnl_cents = (pnl * 100.0).round() as i64;
        self.daily_pnl_cents.fetch_add(pnl_cents, Ordering::SeqCst);

        // Update positions (store dollar cost basis)
        let kalshi_cost = kalshi_contracts as f64 * kalshi_price_cents as f64 / 100.0;
        let poly_cost = poly_contracts as f64 * poly_price_cents as f64 / 100.0;
        let mut positions = self.positions.write().await;
        let pos = positions.entry(market_id.to_string()).or_default();
        pos.total_cost += kalshi_cost + poly_cost;
    }
    
    /// Record an error
    pub async fn record_error(&self) {
        let errors = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;
        
        if errors >= self.config.max_consecutive_errors as i64 {
            self.trip(TripReason::ConsecutiveErrors {
                count: errors as u32,
                limit: self.config.max_consecutive_errors,
            }).await;
        }
    }
    
    /// Record a fill mismatch on a specific market.
    /// Increments the market's consecutive mismatch count and blacklists it
    /// if it exceeds the threshold. Also counts as a consecutive error.
    pub async fn record_mismatch(&self, market_id: &str) {
        // Count as a consecutive error (can trip global halt)
        self.record_error().await;

        if !self.config.enabled {
            return;
        }

        let mut failures = self.market_failures.write().await;
        let state = failures.entry(market_id.to_string()).or_insert(MarketFailureState {
            consecutive_mismatches: 0,
            blacklisted_at: None,
        });

        state.consecutive_mismatches += 1;
        let count = state.consecutive_mismatches;

        if count >= self.market_blacklist_threshold && state.blacklisted_at.is_none() {
            state.blacklisted_at = Some(Instant::now());
            error!(
                "[CB] 🚫 MARKET BLACKLISTED: {} ({} consecutive mismatches, blacklisted for {}s)",
                market_id, count, self.market_blacklist_secs
            );
        } else if state.blacklisted_at.is_none() {
            warn!(
                "[CB] ⚠️ Mismatch #{} on {} (blacklist at {})",
                count, market_id, self.market_blacklist_threshold
            );
        }
    }

    /// Check if a market is currently blacklisted.
    /// Automatically clears expired blacklists.
    pub async fn is_market_blacklisted(&self, market_id: &str) -> bool {
        if !self.config.enabled {
            return false;
        }

        let mut failures = self.market_failures.write().await;
        if let Some(state) = failures.get_mut(market_id) {
            if let Some(blacklisted_at) = state.blacklisted_at {
                if blacklisted_at.elapsed() > Duration::from_secs(self.market_blacklist_secs) {
                    // Blacklist expired - reset
                    info!("[CB] Market {} blacklist expired, re-enabling", market_id);
                    state.consecutive_mismatches = 0;
                    state.blacklisted_at = None;
                    return false;
                }
                return true;
            }
        }
        false
    }

    /// Clear mismatch count for a market after a successful matched execution
    pub async fn clear_market_mismatches(&self, market_id: &str) {
        let mut failures = self.market_failures.write().await;
        if let Some(state) = failures.get_mut(market_id) {
            if state.blacklisted_at.is_none() {
                state.consecutive_mismatches = 0;
            }
        }
    }

    /// Record P&L update (for tracking without execution)
    pub fn record_pnl(&self, pnl: f64) {
        let pnl_cents = (pnl * 100.0).round() as i64;
        self.daily_pnl_cents.fetch_add(pnl_cents, Ordering::SeqCst);
    }

    /// Trip the circuit breaker
    pub async fn trip(&self, reason: TripReason) {
        if !self.config.enabled {
            return;
        }
        
        error!("🚨 CIRCUIT BREAKER TRIPPED: {}", reason);
        
        self.halted.store(true, Ordering::SeqCst);
        *self.tripped_at.write().await = Some(Instant::now());
        *self.trip_reason.write().await = Some(reason);
    }
    
    /// Manually halt trading
    #[allow(dead_code)]
    pub async fn halt(&self) {
        warn!("[CB] Manual halt triggered");
        self.trip(TripReason::ManualHalt).await;
    }

    /// Reset the circuit breaker (after cooldown or manual reset)
    #[allow(dead_code)]
    pub async fn reset(&self) {
        info!("[CB] Circuit breaker reset");
        self.halted.store(false, Ordering::SeqCst);
        *self.tripped_at.write().await = None;
        *self.trip_reason.write().await = None;
        self.consecutive_errors.store(0, Ordering::SeqCst);
    }

    /// Reset daily P&L (call at midnight)
    #[allow(dead_code)]
    pub fn reset_daily_pnl(&self) {
        info!("[CB] Daily P&L reset");
        self.daily_pnl_cents.store(0, Ordering::SeqCst);
    }

    /// Check if cooldown has elapsed and auto-reset if so
    pub async fn check_cooldown(&self) -> bool {
        if !self.halted.load(Ordering::SeqCst) {
            return true;
        }

        let tripped_at = self.tripped_at.read().await;
        if let Some(tripped) = *tripped_at {
            if tripped.elapsed() > Duration::from_secs(self.config.cooldown_secs) {
                drop(tripped_at); // Release read lock before reset
                self.reset().await;
                return true;
            }
        }

        false
    }

    /// Get current status
    #[allow(dead_code)]
    pub async fn status(&self) -> CircuitBreakerStatus {
        let positions = self.positions.read().await;
        let total_position: f64 = positions.values().map(|p| p.total_cost_basis()).sum();

        CircuitBreakerStatus {
            enabled: self.config.enabled,
            halted: self.halted.load(Ordering::SeqCst),
            trip_reason: self.trip_reason.read().await.clone(),
            consecutive_errors: self.consecutive_errors.load(Ordering::SeqCst) as u32,
            daily_pnl: self.daily_pnl_cents.load(Ordering::SeqCst) as f64 / 100.0,
            total_position,
            market_count: positions.len(),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CircuitBreakerStatus {
    pub enabled: bool,
    pub halted: bool,
    pub trip_reason: Option<TripReason>,
    pub consecutive_errors: u32,
    pub daily_pnl: f64,
    pub total_position: f64,
    pub market_count: usize,
}

impl std::fmt::Display for CircuitBreakerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.enabled {
            return write!(f, "Circuit Breaker: DISABLED");
        }

        if self.halted {
            write!(f, "Circuit Breaker: 🛑 HALTED")?;
            if let Some(reason) = &self.trip_reason {
                write!(f, " ({})", reason)?;
            }
        } else {
            write!(f, "Circuit Breaker: ✅ OK")?;
        }

        write!(f, " | P&L: ${:.2} | Pos: ${:.2} across {} markets | Errors: {}",
               self.daily_pnl, self.total_position, self.market_count, self.consecutive_errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.001
    }

    #[tokio::test]
    async fn test_circuit_breaker_position_limit() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 10.0, // $10 limit
            max_total_position: 50.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // 5 contracts at 50c each = $2.50 per leg, $5.00 total cost basis
        assert!(cb.can_execute("market1", 5.0).await.is_ok());

        // Record: 5 kalshi @ 50c + 5 poly @ 50c = $5.00 total
        cb.record_success("market1", 5, 50, 5, 50, 0.0).await;

        // Try to add $8 more (would make $13, exceeding $10 limit)
        let result = cb.can_execute("market1", 8.0).await;
        assert!(matches!(result, Err(TripReason::MaxPositionPerMarket { .. })));
    }

    #[tokio::test]
    async fn test_consecutive_errors() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        cb.record_error().await;
        cb.record_error().await;
        assert!(cb.is_trading_allowed());

        // Third error should trip
        cb.record_error().await;
        assert!(!cb.is_trading_allowed());
    }

    #[tokio::test]
    async fn test_get_remaining_capacity_empty() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0, // $100
            max_total_position: 500.0,      // $500
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        let capacity = cb.get_remaining_capacity("market1").await;
        assert!(approx_eq(capacity.per_market, 100.0));
        assert!(approx_eq(capacity.total, 500.0));
        assert!(approx_eq(capacity.effective, 100.0));
    }

    #[tokio::test]
    async fn test_get_remaining_capacity_after_trade() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // Record: 15 kalshi @ 40c ($6) + 15 poly @ 60c ($9) = $15 total
        cb.record_success("market1", 15, 40, 15, 60, 0.0).await;

        let capacity = cb.get_remaining_capacity("market1").await;
        assert!(approx_eq(capacity.per_market, 85.0));  // 100 - 15
        assert!(approx_eq(capacity.total, 485.0));       // 500 - 15
        assert!(approx_eq(capacity.effective, 85.0));    // min(85, 485)

        // Different market: only limited by total
        let capacity2 = cb.get_remaining_capacity("market2").await;
        assert!(approx_eq(capacity2.per_market, 100.0));
        assert!(approx_eq(capacity2.total, 485.0));
        assert!(approx_eq(capacity2.effective, 100.0));
    }

    #[tokio::test]
    async fn test_get_remaining_capacity_total_limit_constrains() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 50.0, // Total is less than per-market
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        let capacity = cb.get_remaining_capacity("market1").await;
        assert!(approx_eq(capacity.per_market, 100.0));
        assert!(approx_eq(capacity.total, 50.0));
        assert!(approx_eq(capacity.effective, 50.0)); // min(100, 50)
    }

    #[tokio::test]
    async fn test_get_remaining_capacity_disabled() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: false,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        let capacity = cb.get_remaining_capacity("market1").await;
        assert_eq!(capacity.per_market, f64::MAX);
        assert_eq!(capacity.total, f64::MAX);
        assert_eq!(capacity.effective, f64::MAX);
    }

    #[tokio::test]
    async fn test_min_contracts_getter() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 5,
        };

        let cb = CircuitBreaker::new(config);
        assert_eq!(cb.min_contracts(), 5);
    }

    #[tokio::test]
    async fn test_market_blacklist_after_repeated_mismatches() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 10,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        assert!(!cb.is_market_blacklisted("market1").await);

        cb.record_mismatch("market1").await;
        cb.record_mismatch("market1").await;
        assert!(!cb.is_market_blacklisted("market1").await);

        cb.record_mismatch("market1").await;
        assert!(cb.is_market_blacklisted("market1").await);

        assert!(!cb.is_market_blacklisted("market2").await);
    }

    #[tokio::test]
    async fn test_market_blacklist_clears_on_success() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 10,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        cb.record_mismatch("market1").await;
        cb.record_mismatch("market1").await;
        cb.clear_market_mismatches("market1").await;

        cb.record_mismatch("market1").await;
        assert!(!cb.is_market_blacklisted("market1").await);
    }

    #[tokio::test]
    async fn test_mismatch_counts_as_consecutive_error() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        cb.record_mismatch("market1").await;
        cb.record_mismatch("market2").await;
        assert!(cb.is_trading_allowed());

        cb.record_mismatch("market3").await;
        assert!(!cb.is_trading_allowed());
    }

    #[tokio::test]
    async fn test_record_pnl_updates_daily_loss() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 1.0, // $1 limit
            max_consecutive_errors: 10,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        cb.record_pnl(-1.50);

        // Should now be blocked by daily loss (cost_basis doesn't matter, loss does)
        let result = cb.can_execute("market1", 1.0).await;
        assert!(matches!(result, Err(TripReason::MaxDailyLoss { .. })));
    }

    #[tokio::test]
    async fn test_dollar_based_position_tracking() {
        // Verify that 100 contracts at 5c is very different from 100 contracts at 95c
        let config = CircuitBreakerConfig {
            max_position_per_market: 50.0, // $50 limit
            max_total_position: 200.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // 100 contracts at 5c = $5 cost basis per leg, $10 total
        cb.record_success("cheap_market", 100, 5, 100, 5, 0.0).await;
        let cap = cb.get_remaining_capacity("cheap_market").await;
        assert!(approx_eq(cap.per_market, 40.0)); // 50 - 10

        // 10 contracts at 95c = $9.50 per leg, $19 total
        cb.record_success("expensive_market", 10, 95, 10, 95, 0.0).await;
        let cap2 = cb.get_remaining_capacity("expensive_market").await;
        assert!(approx_eq(cap2.per_market, 31.0)); // 50 - 19
    }

    /// Test dollar-to-contract conversion logic (mirrors execution.rs capping)
    #[tokio::test]
    async fn test_dollar_to_contract_conversion() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 15.0, // $15 limit
            max_total_position: 100.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // Simulate execution.rs capping logic:
        // cost_per_contract = (yes_price + no_price) / 100.0
        // capacity_contracts = (capacity.effective / cost_per_contract).floor() as i64

        let capacity = cb.get_remaining_capacity("market1").await;
        assert!(approx_eq(capacity.effective, 15.0));

        // Case 1: yes=40c + no=60c → cost_per_contract = $1.00
        // 15.0 / 1.0 = 15 contracts
        let cost_per_contract = (40.0 + 60.0) / 100.0;
        let capacity_contracts = (capacity.effective / cost_per_contract).floor() as i64;
        assert_eq!(capacity_contracts, 15);

        // Case 2: yes=3c + no=2c → cost_per_contract = $0.05
        // 15.0 / 0.05 = 300 contracts
        let cost_per_contract = (3.0 + 2.0) / 100.0;
        let capacity_contracts = (capacity.effective / cost_per_contract).floor() as i64;
        assert_eq!(capacity_contracts, 300);

        // Case 3: yes=3c + no=4c → cost_per_contract = $0.07
        // 15.0 / 0.07 = 214.28... → floor to 214
        let cost_per_contract = (3.0 + 4.0) / 100.0;
        let capacity_contracts = (capacity.effective / cost_per_contract).floor() as i64;
        assert_eq!(capacity_contracts, 214);

        // Case 4: After a trade, remaining capacity decreases
        cb.record_success("market1", 10, 40, 10, 60, 0.0).await;
        let capacity = cb.get_remaining_capacity("market1").await;
        assert!(approx_eq(capacity.effective, 5.0)); // 15 - 10

        let cost_per_contract = (40.0 + 60.0) / 100.0;
        let capacity_contracts = (capacity.effective / cost_per_contract).floor() as i64;
        assert_eq!(capacity_contracts, 5);
    }

    /// Test boundary prices: 1c (minimum) and 99c (maximum)
    #[tokio::test]
    async fn test_boundary_prices() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 10.0,
            max_total_position: 100.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // 1c price: 100 contracts at 1c = $1.00 per leg
        cb.record_success("cheap", 100, 1, 100, 1, 0.0).await;
        let cap = cb.get_remaining_capacity("cheap").await;
        assert!(approx_eq(cap.per_market, 8.0)); // 10 - 2

        // 99c price: 1 contract at 99c = $0.99 per leg
        cb.record_success("expensive", 1, 99, 1, 99, 0.0).await;
        let cap = cb.get_remaining_capacity("expensive").await;
        assert!(approx_eq(cap.per_market, 8.02)); // 10 - 1.98
    }

    /// Test that disabled CB skips capping (capacity.effective == f64::MAX)
    #[tokio::test]
    async fn test_disabled_cb_skips_capping() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: false,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);
        let capacity = cb.get_remaining_capacity("market1").await;

        // Verify that execution.rs should skip capping when capacity is f64::MAX
        assert_eq!(capacity.effective, f64::MAX);

        // This is the guard used in execution.rs:
        // if capacity.effective < f64::MAX { ... do capping ... }
        // When disabled, this block is skipped entirely, avoiding the
        // f64::MAX / cost_per_contract → infinity → i64::MIN overflow.
        assert!(!(capacity.effective < f64::MAX));
    }

    /// Test PnL rounding (not truncation)
    #[tokio::test]
    async fn test_pnl_rounding() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100.0,
            max_total_position: 500.0,
            max_daily_loss: 100.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
            min_contracts: 1,
        };

        let cb = CircuitBreaker::new(config);

        // -$0.009 should round to -1 cent, not truncate to 0
        cb.record_pnl(-0.009);
        let status = cb.status().await;
        assert!(approx_eq(status.daily_pnl, -0.01));

        // $0.005 should round to 1 cent (round half up)
        cb.record_pnl(0.015);
        let status = cb.status().await;
        assert!(approx_eq(status.daily_pnl, 0.01)); // -0.01 + 0.02 = 0.01
    }
}