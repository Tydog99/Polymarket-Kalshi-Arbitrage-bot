# Hybrid Execution Design

## Overview

Enable the controller to execute arbitrage legs locally when a remote trader isn't connected for a platform, while respecting geo-restrictions via a required `CONTROLLER_PLATFORMS` flag.

## Problem

The current architecture requires TWO remote traders (one per platform) to execute cross-platform arbs. When only one remote trader is connected, arbs are dropped. This is inefficient when the controller machine is authorized to trade on one platform.

## Solution

1. **Platform authorization flag**: `CONTROLLER_PLATFORMS` env var specifies which platforms the controller can execute locally
2. **Hybrid execution**: Route each leg independently - to remote trader if connected, otherwise execute locally if authorized
3. **Shared trading crate**: Consolidate API clients and execution logic to avoid duplication

## Platform Authorization Model

| CONTROLLER_PLATFORMS | Mode | Controller executes locally |
|---------------------|------|----------------------------|
| Not set / empty | Pure router | Nothing - requires remote traders |
| `kalshi` | Hybrid | Kalshi only |
| `polymarket` | Hybrid | Polymarket only |
| `kalshi,polymarket` | Hybrid | Both (remote traders optional) |

**Execution logic per leg:**
```
if remote trader connected for platform → route to remote
else if controller authorized for platform → execute locally
else → cannot execute (abort entire arb)
```

## Crate Structure

### New `trading` crate

```
trading/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── kalshi/
    │   ├── mod.rs
    │   ├── client.rs       # KalshiApiClient
    │   ├── types.rs        # KalshiOrderRequest, KalshiOrderResponse
    │   └── config.rs       # KalshiConfig
    ├── polymarket/
    │   ├── mod.rs
    │   ├── client.rs       # SharedAsyncClient
    │   └── types.rs        # PolyFillAsync, PreparedCreds
    └── execution/
        ├── mod.rs
        ├── leg.rs          # execute_leg()
        └── types.rs        # Platform, OrderAction, OutcomeSide, LegRequest, LegResult
```

### Dependency flow

```
trading (no internal dependencies)
    ↑
    ├── controller (uses trading::*)
    └── trader (uses trading::*, deletes api/)
```

## Execution Types

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Kalshi,
    Polymarket,
}

#[derive(Debug, Clone, Copy)]
pub enum OrderAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy)]
pub enum OutcomeSide {
    Yes,
    No,
}

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

pub struct LegResult {
    pub success: bool,
    pub latency_ns: u64,
    pub error: Option<String>,
}

pub struct ExecutionClients {
    pub kalshi: Option<Arc<KalshiApiClient>>,
    pub polymarket: Option<Arc<SharedAsyncClient>>,
}
```

## Shared Execution Function

```rust
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
        Ok(()) => LegResult { success: true, latency_ns, error: None },
        Err(e) => LegResult { success: false, latency_ns, error: Some(e.to_string()) },
    }
}

async fn execute_kalshi_leg(req: &LegRequest<'_>, clients: &ExecutionClients, dry_run: bool) -> Result<()> {
    let ticker = req.kalshi_ticker.ok_or_else(|| anyhow!("Missing kalshi_ticker"))?;
    let side_str = match req.side { OutcomeSide::Yes => "yes", OutcomeSide::No => "no" };

    if dry_run {
        info!("[KALSHI] DRY RUN {:?} {} {}x @ {}¢", req.action, side_str, req.contracts, req.price_cents);
        return Ok(());
    }

    let client = clients.kalshi.as_ref().ok_or_else(|| anyhow!("Kalshi client not available"))?;
    match req.action {
        OrderAction::Buy => { client.buy_ioc(ticker, side_str, req.price_cents as i64, req.contracts).await?; }
        OrderAction::Sell => { client.sell_ioc(ticker, side_str, req.price_cents as i64, req.contracts).await?; }
    }
    Ok(())
}

async fn execute_poly_leg(req: &LegRequest<'_>, clients: &ExecutionClients, dry_run: bool) -> Result<()> {
    let token = req.poly_token.ok_or_else(|| anyhow!("Missing poly_token"))?;
    let price = (req.price_cents as f64) / 100.0;

    if dry_run {
        info!("[POLY] DRY RUN {:?} {:?} {}x @ {:.2}", req.action, req.side, req.contracts, price);
        return Ok(());
    }

    let client = clients.polymarket.as_ref().ok_or_else(|| anyhow!("Polymarket client not available"))?;
    match req.action {
        OrderAction::Buy => { client.buy_fak(token, price, req.contracts as f64).await?; }
        OrderAction::Sell => { client.sell_fak(token, price, req.contracts as f64).await?; }
    }
    Ok(())
}
```

## Controller Changes

### HybridExecutor

```rust
pub struct HybridExecutor {
    state: Arc<GlobalState>,
    circuit_breaker: Arc<CircuitBreaker>,
    router: RemoteTraderRouter,

    // Local execution capability
    local_platforms: HashSet<Platform>,
    local_clients: ExecutionClients,

    in_flight: Arc<[AtomicU64; 8]>,
    leg_seq: AtomicU64,
    pub dry_run: bool,
}
```

### Execution flow

```rust
let legs = build_legs(&req, &pair, max_contracts, &self.leg_seq);

// Phase 1: Verify all legs can execute
for (platform, _) in &legs {
    let has_remote = self.router.is_connected(*platform).await;
    let has_local = self.local_platforms.contains(platform);

    if !has_remote && !has_local {
        warn!("Cannot execute {:?} leg - no remote trader and not authorized locally", platform);
        self.release_in_flight_delayed(market_id);
        return Ok(());
    }
}

// Phase 2: Execute all legs
for (platform, msg) in legs {
    if self.router.is_connected(platform).await {
        self.router.try_send(platform, msg).await;
    } else {
        self.execute_local_leg(platform, msg).await?;
    }
}
```

### main.rs changes

```rust
fn parse_controller_platforms() -> HashSet<Platform> {
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
```

## Trader Changes

The trader becomes simpler - delete `trader/src/api/` and use shared execution:

```rust
async fn handle_execute_leg(&self, msg: IncomingMessage) -> OutgoingMessage {
    let req = leg_request_from_msg(&msg)?;

    if req.platform != self.platform {
        return OutgoingMessage::LegResult {
            success: false,
            error: Some(format!("Trader is {:?} only", self.platform)),
            ...
        };
    }

    let result = execute_leg(&req, &self.clients, self.dry_run).await;

    OutgoingMessage::LegResult {
        market_id: msg.market_id,
        leg_id: req.leg_id.to_string(),
        platform: req.platform,
        success: result.success,
        latency_ns: result.latency_ns,
        error: result.error,
    }
}
```

## File Changes Summary

### Create
- `trading/Cargo.toml`
- `trading/src/lib.rs`
- `trading/src/kalshi/mod.rs`
- `trading/src/kalshi/client.rs`
- `trading/src/kalshi/types.rs`
- `trading/src/kalshi/config.rs`
- `trading/src/polymarket/mod.rs`
- `trading/src/polymarket/client.rs`
- `trading/src/polymarket/types.rs`
- `trading/src/execution/mod.rs`
- `trading/src/execution/leg.rs`
- `trading/src/execution/types.rs`

### Modify
- `Cargo.toml` (workspace) - Add trading to members
- `controller/Cargo.toml` - Add trading dependency
- `controller/src/main.rs` - Parse CONTROLLER_PLATFORMS, pass to executor
- `controller/src/remote_execution.rs` - Rename to hybrid, add local execution
- `controller/src/lib.rs` - Remove kalshi/polymarket_clob modules
- `trader/Cargo.toml` - Add trading dependency
- `trader/src/trader.rs` - Use trading::execute_leg()
- `trader/src/lib.rs` - Remove api module

### Delete
- `controller/src/kalshi.rs` - Moved to trading
- `controller/src/polymarket_clob.rs` - Moved to trading
- `trader/src/api/kalshi.rs` - Replaced by trading
- `trader/src/api/polymarket.rs` - Replaced by trading
- `trader/src/api/mod.rs` - Replaced by trading

## Usage Examples

```bash
# Pure router mode (requires both remote traders)
dotenvx run -- cargo run --release

# Hybrid: controller handles Kalshi locally, Polymarket via remote trader
CONTROLLER_PLATFORMS=kalshi dotenvx run -- cargo run --release

# Hybrid: controller handles Polymarket locally, Kalshi via remote trader
CONTROLLER_PLATFORMS=polymarket dotenvx run -- cargo run --release

# Full local execution (no remote traders needed)
CONTROLLER_PLATFORMS=kalshi,polymarket dotenvx run -- cargo run --release
```
