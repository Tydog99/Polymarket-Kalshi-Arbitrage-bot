//! A/B test execution strategies for comparing different order execution approaches.
//!
//! Strategies vary in contract sizing and execution sequencing to measure
//! which approach minimizes phantom-arb losses from stale Polymarket orderbooks.

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// =============================================================================
// EXECUTION STRATEGY ENUM
// =============================================================================

/// Execution strategy variants for A/B testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStrategy {
    /// Baseline: `tokio::join!` both legs, full detected size.
    Simultaneous,
    /// `tokio::join!` both legs, sized to `min(yes_size, no_size)`.
    MinDepth,
    /// `tokio::join!` both legs, sized to `min(yes_size, no_size, size_cap)`.
    SizeCapped,
    /// Sequential: await Poly fill first, then fire Kalshi for confirmed amount.
    PolyFirst,
}

impl std::fmt::Display for ExecStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Simultaneous => write!(f, "simultaneous"),
            Self::MinDepth => write!(f, "min_depth"),
            Self::SizeCapped => write!(f, "size_capped"),
            Self::PolyFirst => write!(f, "poly_first"),
        }
    }
}

// =============================================================================
// STRATEGY CONFIG
// =============================================================================

/// Configuration for strategy selection, parsed from environment variables.
pub struct StrategyConfig {
    pub active_strategies: Vec<ExecStrategy>,
    pub size_cap: i64,
}

impl StrategyConfig {
    /// Parse from `EXEC_STRATEGIES` and `EXEC_SIZE_CAP` environment variables.
    pub fn from_env() -> Self {
        let active_strategies = std::env::var("EXEC_STRATEGIES")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.split(',')
                    .filter_map(|v| match v.trim() {
                        "simultaneous" => Some(ExecStrategy::Simultaneous),
                        "min_depth" => Some(ExecStrategy::MinDepth),
                        "size_capped" => Some(ExecStrategy::SizeCapped),
                        "poly_first" => Some(ExecStrategy::PolyFirst),
                        other => {
                            warn!("[STRATEGY] Unknown strategy '{}', ignoring", other);
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![ExecStrategy::Simultaneous]);

        let size_cap = std::env::var("EXEC_SIZE_CAP")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(5);

        info!(
            "[STRATEGY] Active strategies: {:?} | size_cap: {}",
            active_strategies, size_cap
        );

        Self {
            active_strategies,
            size_cap,
        }
    }

    /// Randomly select a strategy from the active set.
    pub fn select(&self) -> ExecStrategy {
        let mut rng = rand::thread_rng();
        *self
            .active_strategies
            .choose(&mut rng)
            .unwrap_or(&ExecStrategy::Simultaneous)
    }
}

// =============================================================================
// STRATEGY RESULT (tracking data)
// =============================================================================

/// Per-execution tracking record, written as one JSONL line.
#[derive(Debug, Serialize, Deserialize)]
pub struct StrategyResult {
    pub strategy: ExecStrategy,
    pub timestamp: String,
    pub market_id: u16,
    pub description: String,
    pub arb_type: String,
    pub contracts_requested: i64,
    pub contracts_executed: i64,
    pub yes_filled: i64,
    pub no_filled: i64,
    pub fill_mismatch: bool,
    pub auto_close_triggered: bool,
    pub profit_cents: i64,
    pub detection_to_exec_ns: u64,
    pub poly_delayed: bool,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_serde_roundtrip() {
        let result = StrategyResult {
            strategy: ExecStrategy::PolyFirst,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            market_id: 42,
            description: "Test market".to_string(),
            arb_type: "poly_yes_kalshi_no".to_string(),
            contracts_requested: 10,
            contracts_executed: 8,
            yes_filled: 8,
            no_filled: 8,
            fill_mismatch: false,
            auto_close_triggered: false,
            profit_cents: 5,
            detection_to_exec_ns: 1000,
            poly_delayed: false,
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: StrategyResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.strategy, ExecStrategy::PolyFirst);
        assert_eq!(parsed.market_id, 42);
    }

    #[test]
    fn test_strategy_display() {
        assert_eq!(ExecStrategy::Simultaneous.to_string(), "simultaneous");
        assert_eq!(ExecStrategy::MinDepth.to_string(), "min_depth");
        assert_eq!(ExecStrategy::SizeCapped.to_string(), "size_capped");
        assert_eq!(ExecStrategy::PolyFirst.to_string(), "poly_first");
    }

    #[test]
    fn test_config_default() {
        // Without env vars set, should default to simultaneous
        let config = StrategyConfig {
            active_strategies: vec![ExecStrategy::Simultaneous],
            size_cap: 5,
        };
        assert_eq!(config.select(), ExecStrategy::Simultaneous);
    }
}
