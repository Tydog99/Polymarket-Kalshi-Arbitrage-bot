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

### Heartbeat Output

The controller displays a heartbeat every 10 seconds (configurable) with market status. The table shows price update deltas since the last heartbeat.

**Default Mode** - Compact table showing markets by league and type:
```
[14:32:05] 💓 234 markets | K:892 P:1847 updates/min
📊 Best opportunity: NBA Game | P_yes(34¢) + K_no(33¢) + K_fee(2¢) = 69¢ | gap=-31¢ | max=50x
┌──────────┬────────────┬────────────┬────────────┬────────────┐
│ League   │ Moneyline  │ Spread     │ Total      │ BTTS       │
├──────────┼────────────┼────────────┼────────────┼────────────┤
│ nba      │ 8 (+42)    │ 12 (+89)   │ 6 (+45)    │ -          │
│ epl      │ 6 (+17)    │ 4 (+23)    │ 3 (+12)    │ 5 (+34)    │
└──────────┴────────────┴────────────┴────────────┴────────────┘
```
The `(+N)` shows price updates since the last heartbeat (delta, not cumulative).

**Configuration:**
```bash
# Change heartbeat interval (default: 10 seconds)
HEARTBEAT_INTERVAL_SECS=5 cargo run -p controller --release
# Or via CLI flag
cargo run -p controller --release -- --heartbeat-interval 5
```

**Verbose Mode** - Hierarchical tree with per-market details:
```bash
# Enable via environment variable
VERBOSE_HEARTBEAT=1 cargo run -p controller --release

# Or via CLI flag
cargo run -p controller --release -- --verbose-heartbeat
```

Shows each market with prices (K:yes/no, P:yes/no), gap from threshold, and update counts.

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
| `VERBOSE_HEARTBEAT` | `false` | `1` or `true` = show detailed hierarchical heartbeat output (or use `--verbose-heartbeat` CLI flag) |
| `HEARTBEAT_INTERVAL_SECS` | `10` | Seconds between heartbeat updates (or use `--heartbeat-interval N` CLI flag) |

### Discovery & Market Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `DISCOVERY_ONLY` | `false` | `1` or `true` = run discovery and exit (useful for testing) |
| `FORCE_DISCOVERY` | `false` | `1` or `true` = clear cache and re-fetch all markets |
| `DISCOVERY_INTERVAL_MINS` | `15` | Minutes between discovery refreshes (0 = disabled) |
| `ENABLED_LEAGUES` | *(all)* | Comma-separated leagues to monitor (see below) |

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

---

## Token Assignment (Kalshi ↔ Polymarket Mapping)

When pairing Kalshi markets with Polymarket, the bot must correctly assign which Polymarket token corresponds to "YES" for the Kalshi market. This is critical - incorrect assignment means betting on the **same team** instead of opposite outcomes.

### The Problem

Kalshi tickers contain a team suffix (e.g., `MUN` in `KXEPLSPREAD-26JAN18MUNEVE-MUN1`), but Polymarket outcomes may be:
- **Team names**: `["Manchester United FC", "Everton FC"]`
- **Yes/No**: `["Yes", "No"]` (team-specific markets like "Will MUN win?")

The suffix `MUN` is **not** a substring of `"Manchester United FC"`, so naive matching fails.

### Token Assignment Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ Polymarket API returns: token1, token2, outcomes                │
└─────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────┐
│ CHECK FIRST: Are outcomes ["Yes", "No"] or ["Over", "Under"]?   │
│                                                                 │
│ Examples:                                                       │
│ - epl-ars-mun-2026-01-25-mun → outcomes=["Yes", "No"]          │
│ - nba-lal-bos-spread-lal-5pt5 → outcomes=["Yes", "No"]         │
│ - nba-lal-bos-total-220pt5 → outcomes=["Over", "Under"]        │
└─────────────────────────────────────────────────────────────────┘
                                   │
                    ┌──────────────┴──────────────┐
                    │                             │
              YES (is Yes/No)               NO (team names)
                    │                             │
                    ▼                             ▼
┌─────────────────────────┐   ┌───────────────────────────────────┐
│ Use API order:          │   │ Match Kalshi suffix to outcomes:  │
│ token1 = YES            │   │ 1. Direct substring match         │
│ token2 = NO             │   │ 2. team_search_terms() lookup     │
│                         │   │ 3. Fallback to API order (warn)   │
└─────────────────────────┘   └───────────────────────────────────┘
```

### Concrete Example: Manchester United Spread

**Phase 1: Kalshi Market Discovery**
```
Ticker: KXEPLSPREAD-26JAN18MUNEVE-MUN1
        └─────────────┬──────────────┘
                      │
Extracted: team1=MUN (away), team2=EVE (home), suffix=MUN1
```

**Phase 2: Polymarket Gamma API Lookup**
```
Slug: epl-mun-eve-2026-01-18-spread-away-1pt5

Response:
  token1: "0x1234abcd..."
  token2: "0x5678efgh..."
  outcomes: ["Manchester United FC", "Everton FC"]
```

**Phase 3: Token Assignment**
```
suffix = "MUN1" → team_code = "mun" (strip numeric)

Direct match: "manchester united fc".contains("mun") = FALSE

Lookup: team_search_terms("epl", "mun") = ["manchester", "man utd", "united"]

Search term match: "manchester united fc".contains("manchester") = TRUE

Result: token1 = YES (Man United), token2 = NO (Everton)
```

### Team Search Terms

For team codes that aren't substrings of full names, `cache::team_search_terms()` provides searchable fragments:

| Code | Team | Search Terms |
|------|------|--------------|
| `MUN` | Manchester United | `["manchester", "man utd", "united"]` |
| `NFO` | Nottingham Forest | `["nottingham", "forest"]` |
| `AVL` | Aston Villa | `["aston", "villa"]` |
| `LAL` | Los Angeles Lakers | `["lakers", "los angeles"]` |
| `KC` | Kansas City Chiefs | `["chiefs", "kansas city"]` |

See `controller/src/cache.rs` for the complete mapping across all supported leagues.

---

## Known Issues & Technical Notes

### Startup Race Condition (Cached Discovery)

When using cached discovery (`FORCE_DISCOVERY=0`), there's a race condition during WebSocket initialization that can cause arbitrage opportunities to be missed:

**Problem**: When markets are loaded instantly from cache, both WebSockets start simultaneously. During initial snapshot loading:

1. Platform A's snapshots arrive → prices set → `check_arbs()` runs → Platform B prices still 0 → no arb detected
2. Platform B's snapshots arrive → prices set → `check_arbs()` runs → but Platform A prices might not be fully loaded yet

By the time all prices are populated, no new WebSocket update triggers a recheck because the `process_price_change` path requires prices to *improve* (go lower).

**Current Fix**: A startup sweep runs 10 seconds after launch, scanning all markets with complete price data and triggering any missed arbitrage opportunities.

**Alternative Fixes** (not yet implemented):

| Fix | Description | Trade-off |
|-----|-------------|-----------|
| **Remove price improvement filter** | Always recheck arbs on any price update in `process_price_change` | Higher CPU usage, more channel traffic |
| **Both-platforms-ready gate** | Only start arb checking after both WebSocket connections confirm initial snapshots complete | Requires connection state tracking, slight delay on all arb detection |

