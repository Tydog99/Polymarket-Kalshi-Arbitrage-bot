# Polymarket-Kalshi Arbitrage Bot

A Rust arbitrage bot that monitors price discrepancies between Kalshi and Polymarket prediction markets.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              CONTROLLER                                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │   Kalshi    │  │ Polymarket  │  │  Discovery  │  │   Arbitrage         │ │
│  │  WebSocket  │  │  WebSocket  │  │   Engine    │  │   Detection         │ │
│  └──────┬──────┘  └──────┬──────┘  └─────────────┘  └──────────┬──────────┘ │
│         │                │                                      │            │
│         └────────┬───────┘                                      │            │
│                  ▼                                              │            │
│         ┌────────────────┐                                      │            │
│         │  Global State  │◄─────────────────────────────────────┘            │
│         │  (Orderbooks)  │                                                   │
│         └────────┬───────┘                                                   │
│                  │                                                           │
│                  ▼                                                           │
│         ┌─────────────────────────────────────────┐                         │
│         │           HybridExecutor                 │                         │
│         │  ┌─────────────────────────────────┐    │                         │
│         │  │     Execution Decision          │    │                         │
│         │  │  CONTROLLER_PLATFORMS set?      │    │                         │
│         │  └──────────┬──────────────────────┘    │                         │
│         │             │                            │                         │
│         │      ┌──────┴──────┐                    │                         │
│         │      ▼             ▼                    │                         │
│         │  ┌───────┐    ┌─────────┐              │                         │
│         │  │ Local │    │ Remote  │              │                         │
│         │  │ Exec  │    │ Router  │              │                         │
│         │  └───┬───┘    └────┬────┘              │                         │
│         └──────┼─────────────┼────────────────────┘                         │
└────────────────┼─────────────┼──────────────────────────────────────────────┘
                 │             │
                 ▼             ▼
        ┌────────────┐  ┌─────────────────┐
        │  trading   │  │  Remote Trader  │
        │   crate    │  │   (WebSocket)   │
        │ ┌────────┐ │  │  ┌───────────┐  │
        │ │ Kalshi │ │  │  │  trading  │  │
        │ │ Client │ │  │  │   crate   │  │
        │ ├────────┤ │  │  └───────────┘  │
        │ │  Poly  │ │  └─────────────────┘
        │ │ Client │ │
        │ └────────┘ │
        └────────────┘
```

## Workspace Crates

| Crate | Description |
|-------|-------------|
| **`controller/`** | Main arbitrage bot - market discovery, WebSocket feeds, arbitrage detection, hybrid execution |
| **`trader/`** | Optional remote trader that receives execution instructions over WebSocket |
| **`trading/`** | Shared library with Kalshi/Polymarket API clients and `execute_leg()` function |
| **`bootstrap/`** | Tailscale setup utility for distributed deployment |

## Hybrid Execution

The controller supports **hybrid execution** - it can execute trades locally OR route them to remote traders:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Arbitrage Opportunity                         │
│                  (Kalshi YES + Poly NO)                         │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                    HybridExecutor                                │
│                                                                  │
│  For each leg, check CONTROLLER_PLATFORMS:                      │
│                                                                  │
│  ┌─────────────┐                      ┌─────────────┐           │
│  │ Kalshi leg  │                      │  Poly leg   │           │
│  └──────┬──────┘                      └──────┬──────┘           │
│         │                                    │                   │
│    ┌────┴────┐                          ┌────┴────┐             │
│    ▼         ▼                          ▼         ▼             │
│ ┌─────┐  ┌────────┐                 ┌─────┐  ┌────────┐         │
│ │Local│  │Remote  │                 │Local│  │Remote  │         │
│ │Exec │  │Trader  │                 │Exec │  │Trader  │         │
│ └─────┘  └────────┘                 └─────┘  └────────┘         │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

**Configuration via `CONTROLLER_PLATFORMS`:**

| Value | Behavior |
|-------|----------|
| (unset) | Route all legs to remote traders |
| `kalshi` | Execute Kalshi locally, route Polymarket to traders |
| `polymarket` | Execute Polymarket locally, route Kalshi to traders |
| `kalshi,polymarket` | Execute all legs locally (no remote traders needed) |

## Getting Started

### Build

```bash
cargo build --workspace --release
```

### Configure

Create `.env` in the repo root:

```bash
# Kalshi credentials
KALSHI_API_KEY_ID=your-key-id
KALSHI_PRIVATE_KEY_PATH=/path/to/private.pem

# Polymarket credentials
POLY_PRIVATE_KEY=0x...
POLY_FUNDER=0x...
POLYMARKET_API_KEY=...
POLYMARKET_API_SECRET=...
POLYMARKET_API_PASSPHRASE=...

# Execution mode
DRY_RUN=1                    # 1=paper trading, 0=live
CONTROLLER_PLATFORMS=        # Empty=remote only, or kalshi,polymarket
```

### Run

```bash
# Dry run (paper trading)
cargo run -p controller --release

# Live execution (local for both platforms)
DRY_RUN=0 CONTROLLER_PLATFORMS=kalshi,polymarket cargo run -p controller --release

# With remote traders (Kalshi local, Poly remote)
DRY_RUN=0 CONTROLLER_PLATFORMS=kalshi cargo run -p controller --release
```

### Quick Smoke Test

```bash
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 cargo run -p controller --release
```

## Remote Trader (Optional)

For distributed execution across machines:

```bash
# On trader machine
cargo run -p remote-trader --release
```

See `trader/README.md` for setup and required environment variables.

