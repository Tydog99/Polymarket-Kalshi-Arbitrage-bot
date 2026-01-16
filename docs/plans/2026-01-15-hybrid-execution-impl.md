# Hybrid Execution Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable the controller to execute arbitrage legs locally when a remote trader isn't connected, using a shared `trading` crate for execution logic.

**Architecture:** Create a new `trading` crate containing Kalshi/Polymarket API clients and a shared `execute_leg()` function. Both controller and trader depend on this crate. The controller's `HybridExecutor` routes legs to remote traders if connected, otherwise executes locally if authorized via `CONTROLLER_PLATFORMS`.

**Tech Stack:** Rust, tokio, reqwest, ethers-rs, WebSocket

---

## Task 1: Create `trading` Crate Skeleton

**Files:**
- Create: `trading/Cargo.toml`
- Create: `trading/src/lib.rs`
- Modify: `Cargo.toml` (workspace root)

**Step 1: Create trading/Cargo.toml**

```toml
[package]
name = "trading"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", features = ["json"] }
base64 = "0.22"
hmac = "0.12"
sha2 = "0.10"
ethers = { version = "2", features = ["legacy"] }
rsa = "0.9"
pkcs1 = "0.7"
arrayvec = "0.7"
```

**Step 2: Create trading/src/lib.rs**

```rust
//! Shared trading execution library for Kalshi and Polymarket.

pub mod execution;
pub mod kalshi;
pub mod polymarket;
```

**Step 3: Add trading to workspace Cargo.toml**

Find the `[workspace]` section and add `"trading"` to the members array.

**Step 4: Create module stubs**

Create empty module files:
- `trading/src/execution/mod.rs` with `pub mod leg; pub mod types;`
- `trading/src/kalshi/mod.rs` with `pub mod client; pub mod config; pub mod types;`
- `trading/src/polymarket/mod.rs` with `pub mod client; pub mod types;`
- `trading/src/execution/leg.rs` (empty)
- `trading/src/execution/types.rs` (empty)
- `trading/src/kalshi/client.rs` (empty)
- `trading/src/kalshi/config.rs` (empty)
- `trading/src/kalshi/types.rs` (empty)
- `trading/src/polymarket/client.rs` (empty)
- `trading/src/polymarket/types.rs` (empty)

**Step 5: Verify crate compiles**

Run: `cargo check -p trading`
Expected: Compiles with no errors

**Step 6: Commit**

```bash
git add trading/ Cargo.toml
git commit -m "feat(trading): create trading crate skeleton"
```

---

## Task 2: Create Execution Types

**Files:**
- Create: `trading/src/execution/types.rs`
- Modify: `trading/src/execution/mod.rs`

**Step 1: Write execution types**

`trading/src/execution/types.rs`:

```rust
//! Shared types for order execution.

use std::sync::Arc;

/// Trading platform identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Kalshi,
    Polymarket,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Kalshi => write!(f, "Kalshi"),
            Platform::Polymarket => write!(f, "Polymarket"),
        }
    }
}

/// Order action (buy or sell)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderAction {
    Buy,
    Sell,
}

impl std::fmt::Display for OrderAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderAction::Buy => write!(f, "BUY"),
            OrderAction::Sell => write!(f, "SELL"),
        }
    }
}

/// Outcome side (yes or no)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeSide {
    Yes,
    No,
}

impl OutcomeSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutcomeSide::Yes => "yes",
            OutcomeSide::No => "no",
        }
    }
}

impl std::fmt::Display for OutcomeSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Request to execute a single leg of an arbitrage
pub struct LegRequest<'a> {
    pub leg_id: &'a str,
    pub platform: Platform,
    pub action: OrderAction,
    pub side: OutcomeSide,
    pub price_cents: u16,
    pub contracts: i64,
    pub kalshi_ticker: Option<&'a str>,
    pub poly_token: Option<&'a str>,
}

/// Result of leg execution
#[derive(Debug)]
pub struct LegResult {
    pub success: bool,
    pub latency_ns: u64,
    pub error: Option<String>,
}

impl LegResult {
    pub fn ok(latency_ns: u64) -> Self {
        Self { success: true, latency_ns, error: None }
    }

    pub fn err(latency_ns: u64, error: impl Into<String>) -> Self {
        Self { success: false, latency_ns, error: Some(error.into()) }
    }
}
```

**Step 2: Update mod.rs**

`trading/src/execution/mod.rs`:

```rust
//! Order execution module.

pub mod leg;
pub mod types;

pub use leg::execute_leg;
pub use types::*;
```

**Step 3: Verify compiles**

Run: `cargo check -p trading`
Expected: Compiles (leg.rs is still empty stub)

**Step 4: Commit**

```bash
git add trading/src/execution/
git commit -m "feat(trading): add execution types"
```

---

## Task 3: Move Kalshi Types

**Files:**
- Create: `trading/src/kalshi/types.rs`
- Read: `controller/src/kalshi.rs` (lines 37-100 for order types)

**Step 1: Copy order types from controller**

Copy `KalshiOrderRequest`, `KalshiOrderResponse`, and related structs from `controller/src/kalshi.rs` to `trading/src/kalshi/types.rs`. Include:
- `KalshiOrderRequest`
- `KalshiOrderResponse`
- `KalshiOrder`

**Step 2: Update imports**

Ensure all `use` statements are correct for the new location.

**Step 3: Update kalshi/mod.rs**

```rust
//! Kalshi API client and types.

pub mod client;
pub mod config;
pub mod types;

pub use client::KalshiApiClient;
pub use config::KalshiConfig;
pub use types::*;
```

**Step 4: Verify compiles**

Run: `cargo check -p trading`
Expected: Compiles

**Step 5: Commit**

```bash
git add trading/src/kalshi/
git commit -m "feat(trading): add Kalshi order types"
```

---

## Task 4: Move Kalshi Config

**Files:**
- Create: `trading/src/kalshi/config.rs`
- Read: `controller/src/kalshi.rs` (KalshiConfig struct and related)

**Step 1: Copy KalshiConfig from controller**

Copy `KalshiConfig` struct and its `from_env()` method.

**Step 2: Add API constants**

```rust
pub const KALSHI_API_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
pub const KALSHI_WS_URL: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";
pub const KALSHI_API_DELAY_MS: u64 = 60;
```

**Step 3: Verify compiles**

Run: `cargo check -p trading`
Expected: Compiles

**Step 4: Commit**

```bash
git add trading/src/kalshi/
git commit -m "feat(trading): add Kalshi config"
```

---

## Task 5: Move Kalshi Client

**Files:**
- Create: `trading/src/kalshi/client.rs`
- Read: `controller/src/kalshi.rs` (KalshiApiClient impl)

**Step 1: Copy KalshiApiClient from controller**

Copy the `KalshiApiClient` struct and its methods:
- `new()`
- `sign()`
- `auth_headers()`
- `get()`, `post()`
- `create_order()`
- `buy_ioc()`, `sell_ioc()`
- `next_order_id()`

**Step 2: Update imports to use local types**

Replace `use crate::...` with `use super::types::...` and `use super::config::...`.

**Step 3: Write a basic test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_request_serialization() {
        let req = KalshiOrderRequest::ioc_buy(
            std::borrow::Cow::Borrowed("TEST-TICKER"),
            "yes",
            50,
            10,
            std::borrow::Cow::Borrowed("order-123"),
        );
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("TEST-TICKER"));
        assert!(json.contains("\"yes_price\":50"));
    }
}
```

**Step 4: Run test**

Run: `cargo test -p trading kalshi`
Expected: PASS

**Step 5: Commit**

```bash
git add trading/src/kalshi/
git commit -m "feat(trading): add Kalshi API client"
```

---

## Task 6: Move Polymarket Types

**Files:**
- Create: `trading/src/polymarket/types.rs`
- Read: `controller/src/polymarket_clob.rs`

**Step 1: Copy types from controller**

Copy from `controller/src/polymarket_clob.rs`:
- `ApiCreds`
- `PreparedCreds`
- `PolyOrderType`
- `PolyFillAsync`
- `PolymarketOrderResponse`
- Order calculation functions: `price_to_bps()`, `size_to_micro()`, `get_order_amounts_buy()`, `get_order_amounts_sell()`, `price_valid()`

**Step 2: Update imports**

Ensure all imports are correct for the new location.

**Step 3: Update polymarket/mod.rs**

```rust
//! Polymarket CLOB client and types.

pub mod client;
pub mod types;

pub use client::{PolymarketAsyncClient, SharedAsyncClient};
pub use types::*;
```

**Step 4: Verify compiles**

Run: `cargo check -p trading`
Expected: Compiles

**Step 5: Commit**

```bash
git add trading/src/polymarket/
git commit -m "feat(trading): add Polymarket types"
```

---

## Task 7: Move Polymarket Client

**Files:**
- Create: `trading/src/polymarket/client.rs`
- Read: `controller/src/polymarket_clob.rs`

**Step 1: Copy clients from controller**

Copy from `controller/src/polymarket_clob.rs`:
- `PolymarketAsyncClient`
- `SharedAsyncClient`
- Helper functions for EIP-712 signing
- Constants (`POLY_CLOB_HOST`, etc.)

**Step 2: Update imports**

Replace `use crate::...` with `use super::types::...`.

**Step 3: Verify compiles**

Run: `cargo check -p trading`
Expected: Compiles

**Step 4: Commit**

```bash
git add trading/src/polymarket/
git commit -m "feat(trading): add Polymarket client"
```

---

## Task 8: Implement execute_leg()

**Files:**
- Modify: `trading/src/execution/leg.rs`
- Modify: `trading/src/execution/types.rs` (add ExecutionClients)

**Step 1: Add ExecutionClients to types.rs**

```rust
use crate::kalshi::KalshiApiClient;
use crate::polymarket::SharedAsyncClient;

/// Clients available for execution
pub struct ExecutionClients {
    pub kalshi: Option<Arc<KalshiApiClient>>,
    pub polymarket: Option<Arc<SharedAsyncClient>>,
}
```

**Step 2: Implement execute_leg**

`trading/src/execution/leg.rs`:

```rust
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
    let ticker = req.kalshi_ticker
        .ok_or_else(|| anyhow!("Missing kalshi_ticker"))?;
    let side_str = req.side.as_str();

    if dry_run {
        info!(
            "[KALSHI] DRY RUN {} {} {}x @ {}¢",
            req.action, side_str, req.contracts, req.price_cents
        );
        return Ok(());
    }

    let client = clients.kalshi.as_ref()
        .ok_or_else(|| anyhow!("Kalshi client not available"))?;

    match req.action {
        OrderAction::Buy => {
            client.buy_ioc(ticker, side_str, req.price_cents as i64, req.contracts).await?;
        }
        OrderAction::Sell => {
            client.sell_ioc(ticker, side_str, req.price_cents as i64, req.contracts).await?;
        }
    }
    Ok(())
}

async fn execute_poly_leg(
    req: &LegRequest<'_>,
    clients: &ExecutionClients,
    dry_run: bool,
) -> Result<()> {
    let token = req.poly_token
        .ok_or_else(|| anyhow!("Missing poly_token"))?;
    let price = (req.price_cents as f64) / 100.0;

    if dry_run {
        info!(
            "[POLY] DRY RUN {} {} {}x @ {:.2}",
            req.action, req.side, req.contracts, price
        );
        return Ok(());
    }

    let client = clients.polymarket.as_ref()
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
            kalshi: None,  // Not needed for dry run
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
            kalshi_ticker: None,  // Missing!
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
```

**Step 3: Run tests**

Run: `cargo test -p trading execution`
Expected: All tests PASS

**Step 4: Commit**

```bash
git add trading/src/execution/
git commit -m "feat(trading): implement execute_leg function"
```

---

## Task 9: Update Controller Dependencies

**Files:**
- Modify: `controller/Cargo.toml`
- Modify: `controller/src/lib.rs`

**Step 1: Add trading dependency to controller**

Add to `controller/Cargo.toml`:

```toml
trading = { path = "../trading" }
```

**Step 2: Update controller/src/lib.rs**

Keep `kalshi` and `polymarket_clob` modules for now (WebSocket code still lives there).
We'll refactor incrementally.

**Step 3: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles

**Step 4: Commit**

```bash
git add controller/Cargo.toml controller/src/lib.rs
git commit -m "feat(controller): add trading crate dependency"
```

---

## Task 10: Add CONTROLLER_PLATFORMS Parsing

**Files:**
- Modify: `controller/src/config.rs`

**Step 1: Add parse function**

Add to `controller/src/config.rs`:

```rust
use std::collections::HashSet;
use trading::execution::Platform;

/// Parse CONTROLLER_PLATFORMS env var.
/// Returns empty set if not set (pure router mode).
pub fn parse_controller_platforms() -> HashSet<Platform> {
    std::env::var("CONTROLLER_PLATFORMS")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|p| match p.trim().to_lowercase().as_str() {
                    "kalshi" => Some(Platform::Kalshi),
                    "polymarket" | "poly" => Some(Platform::Polymarket),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_controller_platforms_empty() {
        std::env::remove_var("CONTROLLER_PLATFORMS");
        let platforms = parse_controller_platforms();
        assert!(platforms.is_empty());
    }

    #[test]
    fn test_parse_controller_platforms_kalshi() {
        std::env::set_var("CONTROLLER_PLATFORMS", "kalshi");
        let platforms = parse_controller_platforms();
        assert_eq!(platforms.len(), 1);
        assert!(platforms.contains(&Platform::Kalshi));
        std::env::remove_var("CONTROLLER_PLATFORMS");
    }

    #[test]
    fn test_parse_controller_platforms_both() {
        std::env::set_var("CONTROLLER_PLATFORMS", "kalshi,polymarket");
        let platforms = parse_controller_platforms();
        assert_eq!(platforms.len(), 2);
        assert!(platforms.contains(&Platform::Kalshi));
        assert!(platforms.contains(&Platform::Polymarket));
        std::env::remove_var("CONTROLLER_PLATFORMS");
    }
}
```

**Step 2: Run tests**

Run: `cargo test -p controller parse_controller`
Expected: All tests PASS

**Step 3: Commit**

```bash
git add controller/src/config.rs
git commit -m "feat(controller): add CONTROLLER_PLATFORMS parsing"
```

---

## Task 11: Create HybridExecutor

**Files:**
- Modify: `controller/src/remote_execution.rs`

**Step 1: Add imports and new fields**

Add to top of file:

```rust
use std::collections::HashSet;
use trading::execution::{execute_leg, ExecutionClients, LegRequest, LegResult, Platform as TradingPlatform};
use trading::kalshi::KalshiApiClient;
use trading::polymarket::SharedAsyncClient;
```

**Step 2: Update RemoteExecutor struct**

Rename to `HybridExecutor` and add fields:

```rust
pub struct HybridExecutor {
    state: Arc<GlobalState>,
    circuit_breaker: Arc<CircuitBreaker>,
    router: RemoteTraderRouter,
    local_platforms: HashSet<TradingPlatform>,
    local_clients: ExecutionClients,
    in_flight: Arc<[AtomicU64; 8]>,
    leg_seq: AtomicU64,
    pub dry_run: bool,
}
```

**Step 3: Update constructor**

```rust
impl HybridExecutor {
    pub fn new(
        state: Arc<GlobalState>,
        circuit_breaker: Arc<CircuitBreaker>,
        router: RemoteTraderRouter,
        local_platforms: HashSet<TradingPlatform>,
        kalshi_api: Option<Arc<KalshiApiClient>>,
        poly_async: Option<Arc<SharedAsyncClient>>,
        dry_run: bool,
    ) -> Self {
        Self {
            state,
            circuit_breaker,
            router,
            local_platforms,
            local_clients: ExecutionClients {
                kalshi: kalshi_api,
                polymarket: poly_async,
            },
            in_flight: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            leg_seq: AtomicU64::new(1),
            dry_run,
        }
    }
}
```

**Step 4: Add helper to convert WsPlatform to TradingPlatform**

```rust
fn ws_to_trading_platform(p: WsPlatform) -> TradingPlatform {
    match p {
        WsPlatform::Kalshi => TradingPlatform::Kalshi,
        WsPlatform::Polymarket => TradingPlatform::Polymarket,
    }
}

fn trading_to_ws_platform(p: TradingPlatform) -> WsPlatform {
    match p {
        TradingPlatform::Kalshi => WsPlatform::Kalshi,
        TradingPlatform::Polymarket => WsPlatform::Polymarket,
    }
}
```

**Step 5: Update process() method**

Replace `can_dispatch` logic with two-phase approach:

```rust
pub async fn process(&self, req: FastExecutionRequest) -> Result<()> {
    // ... existing dedup, market lookup, profit check, circuit breaker ...

    let legs = build_legs(&req, &pair, max_contracts, &self.leg_seq);
    if legs.is_empty() {
        self.release_in_flight_delayed(market_id);
        return Ok(());
    }

    // Phase 1: Verify all legs can execute
    for (ws_platform, _) in &legs {
        let trading_platform = ws_to_trading_platform(*ws_platform);
        let has_remote = self.router.is_connected(*ws_platform).await;
        let has_local = self.local_platforms.contains(&trading_platform);

        if !has_remote && !has_local {
            warn!(
                "[HYBRID] Cannot execute {:?} leg - no remote trader and not authorized locally; dropping arb market_id={}",
                trading_platform, market_id
            );
            self.release_in_flight_delayed(market_id);
            return Ok(());
        }
    }

    // Phase 2: Execute all legs
    for (ws_platform, msg) in legs {
        if self.router.is_connected(ws_platform).await {
            if !self.router.try_send(ws_platform, msg).await {
                warn!(
                    "[HYBRID] Failed to send leg to remote {:?}; market_id={}",
                    ws_platform, market_id
                );
            }
        } else {
            if let Err(e) = self.execute_local_leg(ws_platform, msg).await {
                warn!("[HYBRID] Local execution failed: {}", e);
            }
        }
    }

    self.release_in_flight_delayed(market_id);
    Ok(())
}
```

**Step 6: Add execute_local_leg method**

```rust
async fn execute_local_leg(&self, ws_platform: WsPlatform, msg: IncomingMessage) -> Result<()> {
    let IncomingMessage::ExecuteLeg {
        market_id,
        leg_id,
        platform: _,
        action,
        side,
        price,
        contracts,
        kalshi_market_ticker,
        poly_token,
        pair_id: _,
        description,
    } = msg else {
        return Ok(());
    };

    let trading_platform = ws_to_trading_platform(ws_platform);

    let leg_action = match action {
        OrderAction::Buy => trading::execution::OrderAction::Buy,
        OrderAction::Sell => trading::execution::OrderAction::Sell,
    };
    let leg_side = match side {
        OutcomeSide::Yes => trading::execution::OutcomeSide::Yes,
        OutcomeSide::No => trading::execution::OutcomeSide::No,
    };

    let req = LegRequest {
        leg_id: &leg_id,
        platform: trading_platform,
        action: leg_action,
        side: leg_side,
        price_cents: price,
        contracts,
        kalshi_ticker: kalshi_market_ticker.as_deref(),
        poly_token: poly_token.as_deref(),
    };

    let result = execute_leg(&req, &self.local_clients, self.dry_run).await;

    if result.success {
        info!(
            "[LOCAL] ✅ {:?} leg_id={} desc={:?} latency={}µs",
            trading_platform, leg_id, description, result.latency_ns / 1000
        );
        Ok(())
    } else {
        Err(anyhow!(result.error.unwrap_or_else(|| "unknown".into())))
    }
}
```

**Step 7: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles

**Step 8: Commit**

```bash
git add controller/src/remote_execution.rs
git commit -m "feat(controller): implement HybridExecutor with local execution"
```

---

## Task 12: Update Controller main.rs

**Files:**
- Modify: `controller/src/main.rs`

**Step 1: Import new config function**

Add: `use config::parse_controller_platforms;`

**Step 2: Parse platforms after dry_run check**

```rust
let local_platforms = parse_controller_platforms();

if local_platforms.is_empty() {
    info!("   Mode: PURE ROUTER (no local execution)");
} else {
    info!("   Local platforms: {:?}", local_platforms);
}
```

**Step 3: Conditionally initialize clients**

Modify the client initialization to only create clients for authorized platforms:

```rust
// Only initialize Kalshi client if authorized locally OR needed for WebSocket
let kalshi_api = Arc::new(KalshiApiClient::new(kalshi_config));

// Only initialize Polymarket client if authorized locally
let poly_async = if local_platforms.contains(&trading::execution::Platform::Polymarket) || !remote_mode {
    // ... existing polymarket initialization ...
    Some(poly_async)
} else {
    None
};
```

**Step 4: Update HybridExecutor construction**

```rust
let hybrid_exec = Arc::new(HybridExecutor::new(
    state.clone(),
    circuit_breaker.clone(),
    trader_router,
    local_platforms,
    Some(kalshi_api.clone()),  // or None if not authorized
    poly_async,
    dry_run,
));
```

**Step 5: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles

**Step 6: Commit**

```bash
git add controller/src/main.rs
git commit -m "feat(controller): wire up HybridExecutor with CONTROLLER_PLATFORMS"
```

---

## Task 13: Update Trader to Use Trading Crate

**Files:**
- Modify: `trader/Cargo.toml`
- Modify: `trader/src/trader.rs`
- Modify: `trader/src/lib.rs`

**Step 1: Add trading dependency**

Add to `trader/Cargo.toml`:

```toml
trading = { path = "../trading" }
```

**Step 2: Update trader.rs imports**

Replace API imports with trading crate:

```rust
use trading::execution::{execute_leg, ExecutionClients, LegRequest, LegResult};
use trading::execution::{Platform, OrderAction, OutcomeSide};
use trading::kalshi::KalshiApiClient;
use trading::polymarket::SharedAsyncClient;
```

**Step 3: Update handle_execute_leg to use shared function**

Simplify the method to use `execute_leg()`:

```rust
async fn handle_execute_leg(&mut self, /* params */) -> OutgoingMessage {
    // ... validation ...

    let leg_request = LegRequest {
        leg_id: &leg_id,
        platform: /* convert */,
        action: /* convert */,
        side: /* convert */,
        price_cents: price,
        contracts,
        kalshi_ticker: kalshi_market_ticker.as_deref(),
        poly_token: poly_token.as_deref(),
    };

    let result = execute_leg(&leg_request, &self.clients, self.dry_run).await;

    OutgoingMessage::LegResult {
        market_id,
        leg_id,
        platform,
        success: result.success,
        latency_ns: result.latency_ns,
        error: result.error,
    }
}
```

**Step 4: Verify compiles**

Run: `cargo check -p remote-trader`
Expected: Compiles

**Step 5: Commit**

```bash
git add trader/
git commit -m "feat(trader): use trading crate for execution"
```

---

## Task 14: Delete Duplicate API Code from Trader

**Files:**
- Delete: `trader/src/api/kalshi.rs`
- Delete: `trader/src/api/polymarket.rs`
- Delete: `trader/src/api/mod.rs`
- Modify: `trader/src/lib.rs`

**Step 1: Remove api module from lib.rs**

Remove `pub mod api;` line.

**Step 2: Delete api directory**

```bash
rm -rf trader/src/api/
```

**Step 3: Update any remaining imports in trader**

Search for `use crate::api::` and replace with `use trading::`.

**Step 4: Verify compiles**

Run: `cargo check -p remote-trader`
Expected: Compiles

**Step 5: Run all tests**

Run: `cargo test`
Expected: All tests PASS

**Step 6: Commit**

```bash
git add trader/
git commit -m "refactor(trader): remove duplicate API code, use trading crate"
```

---

## Task 15: Integration Test

**Files:**
- Modify: `controller/tests/integration_tests.rs` (or create new test)

**Step 1: Write integration test for hybrid execution**

```rust
#[tokio::test]
async fn test_hybrid_executor_local_kalshi() {
    // Setup: HybridExecutor with local_platforms = {Kalshi}
    // No remote traders connected
    // Send a PolyYesKalshiNo arb
    // Verify: Arb is dropped (no Poly trader, not authorized locally)
}

#[tokio::test]
async fn test_hybrid_executor_routes_to_remote_when_available() {
    // Setup: HybridExecutor with local_platforms = {Kalshi}
    // Remote Poly trader connected
    // Send a PolyYesKalshiNo arb
    // Verify: Kalshi leg executed locally, Poly leg routed to remote
}
```

**Step 2: Run integration tests**

Run: `cargo test -p controller integration`
Expected: All tests PASS

**Step 3: Commit**

```bash
git add controller/tests/
git commit -m "test(controller): add hybrid executor integration tests"
```

---

## Task 16: Final Cleanup and Verification

**Step 1: Run full test suite**

Run: `cargo test --all`
Expected: All tests PASS

**Step 2: Run clippy**

Run: `cargo clippy --all -- -D warnings`
Expected: No warnings

**Step 3: Build release**

Run: `cargo build --release`
Expected: Builds successfully

**Step 4: Manual smoke test**

```bash
# Test pure router mode (should require remote traders)
DRY_RUN=1 dotenvx run -- cargo run --release -p controller

# Test with local Kalshi
DRY_RUN=1 CONTROLLER_PLATFORMS=kalshi dotenvx run -- cargo run --release -p controller
```

**Step 5: Final commit**

```bash
git add -A
git commit -m "chore: final cleanup for hybrid execution"
```

---

## Summary

| Task | Description |
|------|-------------|
| 1 | Create trading crate skeleton |
| 2 | Create execution types |
| 3 | Move Kalshi types |
| 4 | Move Kalshi config |
| 5 | Move Kalshi client |
| 6 | Move Polymarket types |
| 7 | Move Polymarket client |
| 8 | Implement execute_leg() |
| 9 | Update controller dependencies |
| 10 | Add CONTROLLER_PLATFORMS parsing |
| 11 | Create HybridExecutor |
| 12 | Update controller main.rs |
| 13 | Update trader to use trading crate |
| 14 | Delete duplicate API code from trader |
| 15 | Integration tests |
| 16 | Final cleanup and verification |

**Estimated commits:** 16
