# Remote Trading Architecture

## Overview

The system supports split execution where the controller detects arbitrage and a remote trader executes orders. This enables:

- Running controller on a fast machine for detection
- Running trader on a machine with API credentials
- Geographic separation for latency optimization
- Hybrid mode: some platforms local, others remote

## Components

| Component | File | Role |
|-----------|------|------|
| `RemoteTraderServer` | `remote_trader.rs` | WebSocket server on controller |
| `RemoteTraderRouter` | `remote_trader.rs` | Routes messages to connected traders |
| `HybridExecutor` | `remote_execution.rs` | Decides local vs remote execution |
| Protocol types | `remote_protocol.rs` | Shared message definitions |

## Connection Model

```
┌─────────────────┐         WebSocket          ┌─────────────────┐
│   Controller    │◄──────────────────────────►│     Trader      │
│  (WS Server)    │                            │   (WS Client)   │
│                 │                            │                 │
│ - Detects arbs  │                            │ - Has API keys  │
│ - Sends orders  │                            │ - Executes legs │
│ - Tracks state  │                            │ - Reports fills │
└─────────────────┘                            └─────────────────┘
        │                                              │
        │ Tailscale (optional)                         │
        └──────────────────────────────────────────────┘
```

**Key insight**: Trader connects TO controller (not the other way around). This works across NAT since controller just needs to be reachable.

## Protocol Messages

### Controller → Trader

**Init** (sent on connection):
```json
{
  "type": "init",
  "platforms": ["kalshi", "polymarket"],
  "dry_run": false
}
```

**ExecuteLeg** (single platform order):
```json
{
  "type": "execute_leg",
  "market_id": 42,
  "leg_id": "leg_001",
  "platform": "kalshi",
  "action": "buy",
  "side": "yes",
  "price": 52,
  "contracts": 10,
  "kalshi_market_ticker": "KXNBA-...",
  "pair_id": "nba_lal_bos_ml",
  "description": "Lakers vs Celtics ML"
}
```

### Trader → Controller

**LegResult** (execution outcome):
```json
{
  "type": "leg_result",
  "leg_id": "leg_001",
  "platform": "kalshi",
  "success": true,
  "filled": 10,
  "avg_price": 52,
  "order_id": "ord_abc123",
  "error": null
}
```

**Ping/Pong** (keepalive):
```json
{"type": "ping"}
{"type": "pong"}
```

## HybridExecutor Logic

The `HybridExecutor` checks each leg and decides execution path:

```rust
for (platform, leg) in legs {
    let has_remote = router.is_connected(platform).await;
    let has_local = local_platforms.contains(&platform);

    if has_local {
        // Execute locally using trading crate
        execute_leg(&local_clients, leg).await;
    } else if has_remote {
        // Send to remote trader
        router.send(platform, ExecuteLeg { ... }).await;
    } else {
        // Cannot execute - abort arb
        return Err("No execution path");
    }
}
```

## Configuration

### Environment Variables

| Variable | Example | Description |
|----------|---------|-------------|
| `CONTROLLER_PLATFORMS` | `kalshi` | Platforms to execute locally |
| `WEBSOCKET_URL` | `ws://192.168.1.5:9001` | Manual trader URL (bypasses Tailscale) |

### Platform Combinations

```bash
# Pure router mode - all trades to remote trader
dotenvx run -- cargo run --release

# Kalshi local, Polymarket remote
CONTROLLER_PLATFORMS=kalshi dotenvx run -- cargo run --release

# Both platforms local (no remote trader needed)
CONTROLLER_PLATFORMS=kalshi,polymarket dotenvx run -- cargo run --release
```

## Tailscale Discovery

When using Tailscale, the controller broadcasts UDP beacons and traders auto-discover:

```
Controller                          Trader
    │                                  │
    │──── UDP beacon (port 9000) ─────►│
    │     "arb-controller:9001"        │
    │                                  │
    │◄─── WebSocket connect ───────────│
    │     ws://100.x.x.x:9001          │
    │                                  │
```

### Setup

```bash
# On each machine
cargo run -p bootstrap

# Prompts for:
# - Role: controller or trader
# - Writes config to ~/.arb/config.toml
```

### Config File (`~/.arb/config.toml`)

```toml
role = "controller"  # or "trader"
beacon_port = 9000   # UDP discovery port
ws_port = 9001       # WebSocket port
```

## Sequence Diagram: Arb Execution

```
Controller              Router              Trader
    │                     │                    │
    │ detect arb          │                    │
    │─────────────────────►                    │
    │                     │                    │
    │ ExecuteLeg(kalshi)  │                    │
    │─────────────────────►────────────────────►
    │                     │                    │
    │ ExecuteLeg(poly)    │                    │
    │─────────────────────►────────────────────►
    │                     │                    │
    │                     │   LegResult(kalshi)│
    │◄────────────────────┼────────────────────│
    │                     │                    │
    │                     │   LegResult(poly)  │
    │◄────────────────────┼────────────────────│
    │                     │                    │
    │ record fills        │                    │
    │                     │                    │
```

## Error Handling

| Scenario | Handling |
|----------|----------|
| Trader disconnects | Arbs for that platform rejected until reconnect |
| Leg fails | Other leg may succeed (triggers auto-close) |
| Network timeout | Trader has internal timeout, reports failure |
| Message parse error | Logged, connection may be dropped |

## Debugging

**Check trader connection:**
```rust
let connected = router.is_connected(Platform::Kalshi).await;
```

**Log prefixes:**
- `[HYBRID]` - HybridExecutor decisions
- `[REMOTE_EXEC]` - Leg routing
- `[WS]` - WebSocket connection events

**Test without real trader:**
```rust
// In integration tests
let rx = router.test_register(Platform::Kalshi).await;
// Now controller thinks Kalshi trader is connected
// rx receives messages sent to "Kalshi trader"
```

## Security Notes

1. **No auth on WebSocket**: Relies on Tailscale network isolation
2. **Credentials stay on trader**: Controller never sees API keys
3. **Dry run propagates**: `dry_run: true` in Init prevents real orders
