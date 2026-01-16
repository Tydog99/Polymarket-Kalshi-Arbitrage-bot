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

---

## Environment Variables Reference

### Required Credentials (Controller)

| Variable | Description |
|----------|-------------|
| `KALSHI_API_KEY_ID` | Kalshi API key ID |
| `KALSHI_PRIVATE_KEY_PATH` | Path to Kalshi PEM private key file (default: `kalshi_private_key.txt`) |
| `POLY_PRIVATE_KEY` | Polymarket private key (0x-prefixed) for EIP-712 signing |
| `POLY_FUNDER` | Polymarket wallet address |

### Execution Mode

| Variable | Default | Description |
|----------|---------|-------------|
| `DRY_RUN` | `1` | `1` or `true` = paper trading, `0` = live execution |
| `RUST_LOG` | `info` | Logging level (`debug`, `info`, `warn`, `error`) |

### Discovery & Market Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `DISCOVERY_ONLY` | `false` | `1` or `true` = run discovery and exit (useful for testing) |
| `FORCE_DISCOVERY` | `false` | `1` or `true` = clear cache and re-fetch all markets |
| `DISCOVERY_INTERVAL_MINS` | `15` | Minutes between discovery refreshes (0 = disabled) |
| `ENABLED_LEAGUES` | *(all)* | Comma-separated leagues to monitor (see below) |
| `PRICE_LOGGING` | `false` | `1` or `true` = enable detailed price logging (performance impact) |

**Supported leagues**: `epl`, `bundesliga`, `laliga`, `seriea`, `ligue1`, `ucl`, `uel`, `eflc`, `nba`, `nfl`, `nhl`, `mlb`, `mls`, `ncaaf`, `cs2`, `lol`, `cod`

Example: `ENABLED_LEAGUES=cs2,lol,cod` to monitor only esports.

### Test Mode (Synthetic Arbitrage)

| Variable | Default | Description |
|----------|---------|-------------|
| `TEST_ARB` | `false` | `1` or `true` = inject synthetic arbitrage opportunities |
| `TEST_ARB_TYPE` | `poly_yes_kalshi_no` | Type of synthetic arb to inject |

**TEST_ARB_TYPE values**:
- `poly_yes_kalshi_no` (or `pykn`, `0`) - Buy Polymarket YES + Kalshi NO
- `kalshi_yes_poly_no` (or `kypn`, `1`) - Buy Kalshi YES + Polymarket NO
- `poly_only` (or `poly`, `2`) - Both sides on Polymarket
- `kalshi_only` (or `kalshi`, `3`) - Both sides on Kalshi

### Circuit Breaker (Risk Management)

| Variable | Default | Description |
|----------|---------|-------------|
| `CB_ENABLED` | `true` | Enable circuit breaker |
| `CB_MAX_POSITION_PER_MARKET` | `50000` | Max contracts per market |
| `CB_MAX_TOTAL_POSITION` | `100000` | Max total contracts across all markets |
| `CB_MAX_DAILY_LOSS` | `500.0` | Max daily loss in dollars before halting |
| `CB_MAX_CONSECUTIVE_ERRORS` | `5` | Max consecutive errors before halting |
| `CB_COOLDOWN_SECS` | `300` | Cooldown period (seconds) after circuit breaker trips |

### Remote Trading (Controller)

| Variable | Default | Description |
|----------|---------|-------------|
| `REMOTE_TRADER` | `false` | `1` or `true` = enable remote trader mode |
| `REMOTE_TRADER_BIND` | `0.0.0.0:9001` | WebSocket bind address for remote traders |
| `REMOTE_SMOKE_TEST` | `false` | `1` or `true` = smoke test mode (send synthetic execute, exit) |
| `TRADER_PLATFORMS` | `kalshi,polymarket` | Platforms controller monitors for price feeds |

### Remote Trader Client

| Variable | Default | Description |
|----------|---------|-------------|
| `WEBSOCKET_URL` | *(auto-discover)* | Direct controller URL (e.g., `ws://192.168.1.100:9001`), skips Tailscale discovery |
| `TRADER_PLATFORM` | *(auto-detect)* | Restrict to single platform: `kalshi` or `polymarket` |
| `ONE_SHOT` | `false` | `1` or `true` = exit after first execution |

**Trader credentials** (only needed if running remote trader):

| Variable | Description |
|----------|-------------|
| `KALSHI_API_KEY` | Kalshi API key (trader) |
| `KALSHI_PRIVATE_KEY` | Kalshi private key (trader) |
| `POLYMARKET_PRIVATE_KEY` | Polymarket private key (trader) |
| `POLYMARKET_API_KEY` | Polymarket Gamma API key |
| `POLYMARKET_API_SECRET` | Polymarket API secret |
| `POLYMARKET_FUNDER` | Polymarket wallet address (falls back to `POLY_FUNDER`) |

