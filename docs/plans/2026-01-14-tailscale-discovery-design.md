# Tailscale Auto-Discovery Design

## Overview

Automatic peer discovery and connection setup for controller/trader communication over Tailscale VPN. Eliminates manual IP configuration and provides a guided bootstrap experience.

## Goals

1. **Eliminate manual IP configuration** - No copying Tailscale IPs into env vars
2. **Automated setup verification** - App checks Tailscale is running and connected before starting
3. **Full bootstrap experience** - CLI guides through Tailscale verification and role selection
4. **Peer discovery** - Controller and trader find each other automatically via UDP beacon

## Crate Structure

```
ricks-shit/
├── controller/
├── trader/
├── bootstrap/          ← NEW: Setup CLI
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
├── tailscale/          ← NEW: Shared library
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── status.rs   # Query tailscale status --json
│       ├── beacon.rs   # UDP announcement send/receive
│       └── verify.rs   # Startup verification checks
└── Cargo.toml          (workspace)
```

## Config File

Location: `~/.arb/config.toml`

```toml
role = "controller"  # or "trader"
beacon_port = 9000   # UDP port for discovery
ws_port = 9001       # WebSocket port (controller only)
```

## Beacon Protocol

**Payload (UDP, JSON):**
```json
{
  "role": "controller",
  "ws_port": 9001,
  "version": "0.1.0"
}
```

**Behavior:**
- Controller sends beacon every 2 seconds to all Tailscale peers on port 9000
- Beacon stops when trader connects, resumes on disconnect
- Trader listens on UDP 9000, accepts first valid beacon

## Tailscale Verification

**`tailscale/src/verify.rs`:**

```rust
pub struct TailscaleStatus {
    pub running: bool,
    pub connected: bool,
    pub self_ip: Option<Ipv4Addr>,
    pub peers: Vec<Ipv4Addr>,
}

pub async fn verify() -> Result<TailscaleStatus, TailscaleError>;
```

**Checks:**
1. Run `tailscale status --json`
2. Verify `BackendState == "Running"`
3. Extract self IP from `Self.TailscaleIPs[0]`
4. Extract online peer IPs from `Peer` map

**Errors:**
```rust
pub enum TailscaleError {
    NotInstalled,
    DaemonNotRunning,
    NotConnected,
    NoPeers,
    CommandFailed(String),
}
```

## Beacon Module

**`tailscale/src/beacon.rs`:**

```rust
// Controller side
pub struct BeaconSender {
    socket: UdpSocket,
    peers: Vec<Ipv4Addr>,
    ws_port: u16,
    interval: Duration,  // 2 seconds
}

impl BeaconSender {
    pub async fn new(peers: Vec<Ipv4Addr>, ws_port: u16) -> Result<Self>;
    pub async fn run(&self, stop: CancellationToken);
}

// Trader side
pub struct BeaconListener {
    socket: UdpSocket,
}

pub struct ControllerInfo {
    pub ip: Ipv4Addr,
    pub ws_port: u16,
    pub version: String,
}

impl BeaconListener {
    pub async fn new(port: u16) -> Result<Self>;
    pub async fn wait_for_controller(&self) -> Result<ControllerInfo>;
}
```

## Bootstrap CLI Flow

```
$ cargo run -p bootstrap

=== Arbitrage Bot Setup ===

Checking Tailscale... ✓ Connected as 100.64.0.1
Found 1 peer(s): [100.64.0.2]

What role is this machine?
  1) Controller (runs arbitrage detection)
  2) Trader (executes trades)
> 1

✓ Config written to ~/.arb/config.toml

Start controller now? [Y/n]
> y

[launches controller binary]
```

## Startup Flow

### Controller Startup

1. Load `~/.arb/config.toml`
2. Verify Tailscale daemon running
3. Verify Tailscale connected
4. Get Tailscale interface IP
5. Query peers via `tailscale status --json`
6. Start WebSocket server on `0.0.0.0:9001`
7. Start beacon sender (UDP to all peers every 2s)
8. When trader connects → stop beacon
9. When trader disconnects → resume beacon

### Trader Startup

1. Load `~/.arb/config.toml`
2. Verify Tailscale daemon running
3. Verify Tailscale connected
4. Listen for UDP beacon on port 9000
5. Log: "Waiting for controller..."
6. Receive beacon → extract controller IP and port
7. Connect WebSocket to `ws://{controller_ip}:{ws_port}`
8. Normal operation

### Reconnection

If trader loses WebSocket connection:
1. Close socket
2. Return to beacon listener
3. Controller resumes beaconing on disconnect detection
4. Trader reconnects when beacon received

## Dependencies

- `tokio` - Async UDP, process spawning
- `serde` / `serde_json` - Parse tailscale output, beacon payload
- `directories` - Cross-platform `~/.arb/` path
- `toml` - Config file read/write
- `tokio-util` - CancellationToken for beacon control

## Integration Points

**Controller (`controller/src/main.rs`):**
- Add `tailscale::verify()` at startup
- Add `BeaconSender` task with cancellation on trader connect

**Trader (`trader/src/main.rs`):**
- Add `tailscale::verify()` at startup
- Replace hardcoded `WEBSOCKET_URL` with `BeaconListener::wait_for_controller()`
