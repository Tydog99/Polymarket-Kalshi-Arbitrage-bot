//! Shared leg execution logic.

use anyhow::{anyhow, Result};
use std::time::Instant;
use tracing::info;

use super::types::*;

/// Execute a single leg of an arbitrage trade.
///
/// This function is used by both the controller (for local execution)
/// and the trader (for remote execution).
pub async fn execute_leg(
    req: &LegRequest<'_>,
    clients: &ExecutionClients,
    dry_run: bool,
) -> LegResult {
    let start = Instant::now();

    let result = match req.platform {
        Platform::Kalshi => execute_kalshi_leg(req, clients, dry_run).await,
        Platform::Polymarket => execute_poly_leg(req, clients, dry_run).await,
    };

    let latency_ns = start.elapsed().as_nanos() as u64;

    match result {
        Ok(()) => LegResult::ok(latency_ns),
        Err(e) => LegResult::err(latency_ns, e.to_string()),
    }
}

async fn execute_kalshi_leg(
    req: &LegRequest<'_>,
    clients: &ExecutionClients,
    dry_run: bool,
) -> Result<()> {
    let ticker = req
        .kalshi_ticker
        .ok_or_else(|| anyhow!("Missing kalshi_ticker"))?;
    let side_str = req.side.as_str();

    if dry_run {
        info!(
            "[KALSHI] DRY RUN {} {} {}x @ {}c",
            req.action, side_str, req.contracts, req.price_cents
        );
        return Ok(());
    }

    let client = clients
        .kalshi
        .as_ref()
        .ok_or_else(|| anyhow!("Kalshi client not available"))?;

    match req.action {
        OrderAction::Buy => {
            client
                .buy_ioc(ticker, side_str, req.price_cents as i64, req.contracts)
                .await?;
        }
        OrderAction::Sell => {
            client
                .sell_ioc(ticker, side_str, req.price_cents as i64, req.contracts)
                .await?;
        }
    }
    Ok(())
}

async fn execute_poly_leg(
    req: &LegRequest<'_>,
    clients: &ExecutionClients,
    dry_run: bool,
) -> Result<()> {
    let token = req
        .poly_token
        .ok_or_else(|| anyhow!("Missing poly_token"))?;
    let price = (req.price_cents as f64) / 100.0;

    if dry_run {
        info!(
            "[POLY] DRY RUN {} {} {}x @ {:.2}",
            req.action, req.side, req.contracts, price
        );
        return Ok(());
    }

    let client = clients
        .polymarket
        .as_ref()
        .ok_or_else(|| anyhow!("Polymarket client not available"))?;

    match req.action {
        OrderAction::Buy => {
            client.buy_fak(token, price, req.contracts as f64).await?;
        }
        OrderAction::Sell => {
            client.sell_fak(token, price, req.contracts as f64).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_leg_dry_run_kalshi() {
        let req = LegRequest {
            leg_id: "test-leg",
            platform: Platform::Kalshi,
            action: OrderAction::Buy,
            side: OutcomeSide::Yes,
            price_cents: 50,
            contracts: 10,
            kalshi_ticker: Some("TEST-TICKER"),
            poly_token: None,
        };
        let clients = ExecutionClients {
            kalshi: None,
            polymarket: None,
        };

        let result = execute_leg(&req, &clients, true).await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_execute_leg_dry_run_poly() {
        let req = LegRequest {
            leg_id: "test-leg",
            platform: Platform::Polymarket,
            action: OrderAction::Buy,
            side: OutcomeSide::No,
            price_cents: 45,
            contracts: 5,
            kalshi_ticker: None,
            poly_token: Some("0xtoken"),
        };
        let clients = ExecutionClients {
            kalshi: None,
            polymarket: None,
        };

        let result = execute_leg(&req, &clients, true).await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_execute_leg_missing_ticker() {
        let req = LegRequest {
            leg_id: "test-leg",
            platform: Platform::Kalshi,
            action: OrderAction::Buy,
            side: OutcomeSide::Yes,
            price_cents: 50,
            contracts: 10,
            kalshi_ticker: None, // Missing!
            poly_token: None,
        };
        let clients = ExecutionClients {
            kalshi: None,
            polymarket: None,
        };

        let result = execute_leg(&req, &clients, false).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Missing kalshi_ticker"));
    }
}
