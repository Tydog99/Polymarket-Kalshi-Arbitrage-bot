//! Execution engine for order placement on trading platforms

use anyhow::Result;
use async_trait::async_trait;

use crate::protocol::{ArbType, Platform};

/// Result of an order execution
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub market_id: u16,
    pub success: bool,
    pub profit_cents: i16,
    pub latency_ns: u64,
    pub error: Option<String>,
}

/// Order execution request
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub market_id: u16,
    pub arb_type: ArbType,
    pub yes_price: u16,
    pub no_price: u16,
    pub yes_size: u16,
    pub no_size: u16,
}

impl OrderRequest {
    /// Calculate profit in cents
    pub fn profit_cents(&self) -> i16 {
        let yes_cost = (self.yes_price as i16 * self.yes_size as i16) / 100;
        let no_cost = (self.no_price as i16 * self.no_size as i16) / 100;
        let total_payout = (self.yes_size + self.no_size) as i16;
        total_payout - yes_cost - no_cost
    }
}

/// Trait for platform-specific execution engines
#[async_trait]
pub trait ExecutionEngine: Send + Sync {
    /// Execute an order based on the request
    async fn execute_order(
        &self,
        request: &OrderRequest,
        dry_run: bool,
    ) -> Result<ExecutionResult>;

    /// Check if the engine can execute (circuit breaker, etc.)
    async fn can_execute(&self, market_id: u16, size: i64) -> Result<()>;

    /// Get the platform this engine handles
    fn platform(&self) -> Platform;
}

/// Create execution engines for the specified platforms
pub async fn create_engines(
    platforms: &[Platform],
    config: &crate::config::Config,
) -> Result<Vec<Box<dyn ExecutionEngine>>> {
    let mut engines: Vec<Box<dyn ExecutionEngine>> = Vec::new();

    for platform in platforms {
        match platform {
            Platform::Kalshi => {
                if config.has_kalshi_creds() {
                    engines.push(Box::new(
                        crate::execution::kalshi::KalshiEngine::new(config)?,
                    ));
                } else {
                    anyhow::bail!("Kalshi credentials not available");
                }
            }
            Platform::Polymarket => {
                if config.has_polymarket_creds() {
                    engines.push(Box::new(
                        crate::execution::polymarket::PolymarketEngine::new(config).await?,
                    ));
                } else {
                    anyhow::bail!("Polymarket credentials not available");
                }
            }
        }
    }

    Ok(engines)
}

pub mod kalshi;
pub mod polymarket;
