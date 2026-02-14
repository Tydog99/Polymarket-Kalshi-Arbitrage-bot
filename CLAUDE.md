# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run Commands

```bash
# Build release
cargo build --release

# Run with environment variables (dry run by default)
dotenvx run -- cargo run --release

# Run live trading
DRY_RUN=0 dotenvx run -- cargo run --release

# Run tests
cargo test

# Run benchmarks
cargo bench

# Force market re-discovery (clears cache)
FORCE_DISCOVERY=1 dotenvx run -- cargo run --release

# Test mode with synthetic arbitrage
TEST_ARB=1 TEST_ARB_TYPE=poly_yes_kalshi_no dotenvx run -- cargo run --release

# Single-event mode (monitor only one Kalshi event)
dotenvx run -- cargo run --release -- --kalshi-ticker KXNBAGAME-26JAN24MIAUTA
```

## Architecture Overview

This is a Rust arbitrage bot that monitors price discrepancies between Kalshi and Polymarket prediction markets. The core principle: in prediction markets, YES + NO = $1.00. Arbitrage exists when buying YES on one platform and NO on another costs less than $1.00.

### Data Flow

```
WebSocket Price Updates (kalshi.rs, polymarket.rs)
    ↓
Global State with Lock-Free Orderbook Cache (types.rs)
    ↓
ArbOpportunity::detect() in WebSocket Handlers - validates prices, fees, sizes
    ↓
Confirmation Queue (confirm_queue.rs) or Direct Execution
    ↓
Execution Loop (execution.rs)
    ↓
Platform Orders (kalshi.rs, polymarket_clob.rs)
    ↓
Position Tracking (position_tracker.rs)

Parallel: Heartbeat Monitoring (main.rs, every 10s) - logs status, does NOT trigger execution
```

### Key Modules

- **`main.rs`** - Entry point, WebSocket orchestration, startup sweep, heartbeat monitoring
- **`arb.rs`** - Centralized arbitrage opportunity detection (ArbConfig, ArbOpportunity)
- **`types.rs`** - Core data structures including `AtomicOrderbook` (lock-free using packed u64 with CAS loops)
- **`execution.rs`** - Concurrent order execution with in-flight deduplication (8-slot bitmask for 512 markets)
- **`kalshi.rs`** - Kalshi REST/WebSocket client with RSA signature authentication
- **`polymarket.rs`** - Polymarket WebSocket client and Gamma API integration
- **`polymarket_clob.rs`** - Polymarket CLOB execution with EIP-712 and HMAC-SHA256 signing
- **`discovery.rs`** - Cross-platform market matching with persistent caching (2-hour TTL)
- **`circuit_breaker.rs`** - Risk management: position limits, daily loss limits, error tracking, cooldown, per-market blacklisting
- **`position_tracker.rs`** - Fill recording, P&L calculation, state persistence to `positions.json`
- **`cache.rs`** - Team code bidirectional mapping between platforms
- **`config.rs`** - League definitions, API endpoints, platform configuration
- **`confirm_tui.rs`** - Split-pane TUI for manual arb confirmation with TUI-aware log routing
- **`confirm_queue.rs`** - Queue of pending arbs awaiting user confirmation

### Finding CLI Flags & Environment Variables

**When asked about CLI flags or env vars, check these locations BEFORE suggesting new ones:**

1. **`README.md`** - Full documentation of all CLI flags and env vars with examples
2. **`main.rs:575-650`** - CLI argument parsing (`cli_arg_value`, `cli_has_flag` calls)
3. **`config.rs`** - Env var readers (search for `std::env::var`)

Common pattern: env vars often have corresponding CLI flags (e.g., `CONFIRM_MODE_SKIP` ↔ `--confirm-mode-skip`).

### Detailed Documentation

**Read these docs BEFORE modifying the related subsystems:**

| Doc | Read when... |
|-----|--------------|
| `controller/docs/EXECUTION.md` | Touching `execution.rs`, in-flight dedup, auto-close logic, fill handling |
| `controller/docs/ATOMIC_ORDERBOOK.md` | Touching `types.rs`, `AtomicOrderbook`, bit-packing, or lock-free updates |
| `controller/docs/REMOTE_TRADING.md` | Touching `remote_*.rs`, hybrid executor, controller↔trader protocol |
| `controller/docs/CONFIRMATION_TUI.md` | Touching TUI, logging, `confirm_*.rs`, or debugging log display issues |
| `controller/docs/PAIRING_DEBUG_PLAYBOOK.md` | Debugging market pairing between Kalshi/Polymarket |

### Arbitrage Types

| Type | Description |
|------|-------------|
| `poly_yes_kalshi_no` | Buy Polymarket YES + Kalshi NO |
| `kalshi_yes_poly_no` | Buy Kalshi YES + Polymarket NO |
| `poly_only` | Both sides on Polymarket (rare) |
| `kalshi_only` | Both sides on Kalshi (rare) |

### Auto-Close on Mismatched Fills

When one leg of an arbitrage fills but the other fails (creating unhedged exposure), the system automatically attempts to close the position:

1. **Detection**: After concurrent execution, if `yes_filled != no_filled`, excess exposure exists
2. **Settlement Wait**: For Polymarket, waits 2 seconds for on-chain settlement
3. **Retry with Price Improvement**: Walks down the book 1¢ at a time until filled or hits 1¢ floor

```
[EXEC] 🔄 Waiting 2s for Poly settlement before auto-close (5 yes contracts)
[EXEC] 🔄 Poly close attempt #1: filled 3/5 @ 52c (total: 3/5)
[EXEC] 🔄 Stepping down to 51c (2 contracts remaining)
[EXEC] 🔄 Poly close attempt #2: filled 2/2 @ 51c (total: 5/5)
[EXEC] ✅ Closed 5 Poly contracts for 258¢ (P&L: -7¢)
```

**Configuration** (in `execution.rs`):
- `MIN_PRICE_CENTS = 1` - Floor price, won't go below 1¢
- `PRICE_STEP_CENTS = 1` - Decrement 1¢ per retry
- `RETRY_DELAY_MS = 100` - Brief delay between attempts

### Delayed Polymarket Order Reconciliation

When Polymarket returns `status=delayed` (order accepted but not yet matched), the bot spawns a background task to poll until terminal state instead of triggering premature auto-close.

**Flow:**

1. Poly returns `delayed` → record Kalshi fill with `reconciliation_pending` marker
2. Background task polls Poly order status with exponential backoff
3. On terminal state (filled/cancelled): record Poly fill, clear pending marker
4. On fill mismatch: unwind excess on either platform using auto-close logic

```
[EXEC] Poly order delayed, spawning reconciliation task (order_id: abc123)
[EXEC] Reconciliation polling attempt #1 (delay: 100ms)
[EXEC] Reconciliation polling attempt #2 (delay: 200ms)
[EXEC] Reconciliation complete: 5 contracts filled @ 48c
```

**Configuration:** `POLY_DELAYED_TIMEOUT_MS` (default: 5000) - maximum time to poll before timing out

**Startup Warning:** If the bot restarts with pending reconciliations in `positions.json`, it logs an ERROR banner requiring manual verification:

```
[ERROR] ════════════════════════════════════════════════════════════════
[ERROR] PENDING RECONCILIATIONS DETECTED - Manual verification required!
[ERROR] Market: KXNBAGAME-26JAN24MIAUTA has unresolved delayed orders
[ERROR] ════════════════════════════════════════════════════════════════
```

**Known Limitation:** PolyOnly arbs (both legs on Polymarket) with delayed orders have incomplete reconciliation handling. The system logs a warning but cannot fully reconcile since both sides may be in delayed state. This is a rare edge case.

### Lock-Free Orderbook Design

`AtomicOrderbook` uses a packed u64 format for cache-line efficiency:
- Bit layout: `[yes_ask:16][no_ask:16][yes_size:16][no_size:16]`
- Updates via compare-and-swap loops
- 64-byte aligned for SIMD compatibility

### Fee Calculation

- **Kalshi**: `ceil(0.07 × contracts × price × (1-price))` - factored into arb detection
- **Polymarket**: Zero trading fees

### Polymarket neg_risk Cache

Polymarket uses two different exchange contracts depending on a market's `negRisk` flag. The correct contract address must be embedded in signed orders.

**Data Flow:**
1. **Discovery** (`discovery.rs`): Gamma API returns `negRisk` field for each market
2. **MarketPair** (`types.rs`): Stores `neg_risk: bool` per market pair
3. **Startup** (`main.rs`): Calls `populate_cache_from_pairs()` to populate the token→neg_risk cache
4. **Execution** (`polymarket_clob.rs`): Looks up neg_risk by token_id before signing orders

**Cache Miss Handling:**
If a token_id is not in the cache during order execution, a warning is logged and the system falls back to fetching from the `/neg-risk` API endpoint:
```
⚠️ [POLY] neg_risk CACHE MISS for token 1074158267163... - fetching from API (discovery may have failed to populate)
```

This warning indicates a potential bug in discovery - all tokens should be cached at startup.

**Legacy File Cache:**
The file `~/.clob_market_cache.json` (generated by `controller/scripts/build_sports_cache.py`) is still loaded as a fallback but is no longer required. Discovery-populated cache takes precedence.

## Environment Variables

**Required credentials:**
- `KALSHI_API_KEY_ID`, `KALSHI_PRIVATE_KEY_PATH` (PEM file)
- `POLY_PRIVATE_KEY` (0x-prefixed), `POLY_FUNDER` (wallet address)
- `POLY_SIGNATURE_TYPE` (default: 0) - Signature type for Polymarket orders: 0=EOA direct signing, 1=Poly proxy, 2=Gnosis Safe

**Execution:** `DRY_RUN` (default: 1), `RUST_LOG` (default: info)

**Polymarket reconciliation:** `POLY_DELAYED_TIMEOUT_MS` (default: 5000) - timeout for polling delayed order status

**Hybrid execution:** `CONTROLLER_PLATFORMS` - comma-separated list of platforms the controller executes locally
- `kalshi` - Execute Kalshi orders locally
- `polymarket` or `poly` - Execute Polymarket orders locally
- Empty/unset - Pure router mode (all trades sent to remote trader)

```bash
# Execute on Kalshi only (Polymarket sent to remote trader)
CONTROLLER_PLATFORMS=kalshi dotenvx run -- cargo run --release

# Execute on both platforms locally
CONTROLLER_PLATFORMS=kalshi,polymarket dotenvx run -- cargo run --release

# Pure router mode (no local execution)
dotenvx run -- cargo run --release
```

**Arbitrage detection:**
- `ARB_THRESHOLD_CENTS` (default: 99, valid: 1-100) - arb exists when total cost ≤ threshold. Invalid values logged and replaced with default.
- `ARB_MIN_CONTRACTS` (default: 1.0, must be > 0) - minimum executable contracts for valid arb. Invalid values logged and replaced with default.

**Circuit breaker:** `CB_ENABLED`, `CB_MAX_POSITION_PER_MARKET` (default: 500, in dollars — maximum dollar exposure per market), `CB_MAX_TOTAL_POSITION` (default: 1000, in dollars — maximum total dollar exposure across all markets), `CB_MAX_DAILY_LOSS`, `CB_MAX_CONSECUTIVE_ERRORS`, `CB_COOLDOWN_SECS`, `CB_MIN_CONTRACTS` (minimum contracts to execute, trades are capped to remaining dollar capacity), `CB_MARKET_BLACKLIST_THRESHOLD` (default: 3, consecutive mismatches before blacklisting a market), `CB_MARKET_BLACKLIST_SECS` (default: 300, blacklist duration)

**HTTP Capture (for debugging/replay):**
- `CAPTURE_DIR` - Base directory for HTTP capture (unset = disabled)
- `CAPTURE_FILTER` - What to capture: `orders` (default), `all`, or comma-separated path patterns

```bash
# Enable HTTP capture to ./.captures/ with default filter (order endpoints only)
CAPTURE_DIR=./.captures dotenvx run -- cargo run --release

# Capture all HTTP traffic
CAPTURE_DIR=./.captures CAPTURE_FILTER=all dotenvx run -- cargo run --release
```

When capture is enabled:
- A timestamped session subdirectory is created (e.g., `session_2026-01-19_20-15-30/`)
- Each HTTP exchange is saved as a JSON file with request/response details
- A `manifest.json` tracks all captured files in order
- Sensitive headers (Authorization, API keys, etc.) are automatically excluded

## Tailscale Setup (Remote Trading)

For running controller and trader on separate machines:

```bash
# First-time setup (run on each machine)
cargo run -p bootstrap

# This will:
# 1. Verify Tailscale is installed and connected
# 2. Prompt for role (controller/trader)
# 3. Write config to ~/.arb/config.toml
# 4. Optionally launch the appropriate binary
```

**Manual setup alternative:**

```bash
# Install Tailscale
brew install tailscale

# Start daemon and connect
sudo tailscaled
tailscale up

# Verify connection
tailscale status
```

**Configuration file:** `~/.arb/config.toml`

```toml
role = "controller"  # or "trader"
beacon_port = 9000   # UDP port for discovery
ws_port = 9001       # WebSocket port
```

**How it works:**
- Controller sends UDP beacon to all Tailscale peers every 2 seconds
- Trader listens for beacon and auto-discovers controller IP/port
- No manual IP configuration required
- Falls back to `WEBSOCKET_URL` env var if set

## Rate Limits

- Kalshi: 2 requests/second (60ms delay between requests)
- Polymarket Gamma API: 20 concurrent requests via semaphore
- WebSocket ping intervals: 30 seconds (Polymarket), heartbeat every 60 seconds

## Supported Markets

Soccer (EPL, Bundesliga, La Liga, Serie A, Ligue 1, UCL, UEL, EFL Championship), NBA, NFL, NHL, MLB, MLS, NCAAF, NCAAMB (College Basketball), Esports (CS2, LoL, CoD)

## Market Pairing: Kalshi ↔ Polymarket Slug Mapping

This section documents how market tickers are constructed on each platform and how to map between them.

### Kalshi Ticker Format

Kalshi uses a structured ticker format:

```
{SERIES}-{DATE}{TEAM1}{TEAM2}-{SUFFIX}
```

**Team order varies by sport:**
- **US Sports (NBA, NFL, NHL, MLB, etc.):** AWAY-HOME order (team1=away, team2=home)
- **Soccer (EPL, Bundesliga, La Liga, etc.):** HOME-AWAY order (team1=home, team2=away)

**Example (NBA):** `KXNBASPREAD-26JAN17WASDEN-DEN12`

| Component | Value | Meaning |
|-----------|-------|---------|
| Series | `KXNBASPREAD` | NBA spread markets |
| Date | `26JAN17` | January 17, 2026 |
| Team1 (Away) | `WAS` | Washington Wizards (away) |
| Team2 (Home) | `DEN` | Denver Nuggets (home) |
| Suffix | `DEN12` | Denver wins by 12+ points |

**Example (EPL):** `KXEPLML-25DEC25AVLCFC-CFC` (Aston Villa home vs Chelsea away)

**Key insight:** The code uses `home_team_first` config flag to handle this difference - see `config.rs`.

### Polymarket Slug Format

Polymarket uses URL-friendly slugs with the same team order as Kalshi:

```
{league}-{team1}-{team2}-{date}-{market_type}-{qualifier}
```

**Team order matches Kalshi:** US sports = away-home, Soccer = home-away

**Examples (NBA - away first):**
- Moneyline: `nba-was-den-2026-01-17`
- Spread (home favored): `nba-was-den-2026-01-17-spread-home-12pt5`
- Spread (away favored): `nba-okc-mia-2026-01-17-spread-away-8pt5`
- Total: `nba-was-den-2026-01-17-total-230pt5`

**Examples (EPL - home first):**
- Moneyline: `epl-avl-cfc-2025-12-25-avl` (Aston Villa to win)
- Spread: `epl-avl-cfc-2025-12-25-spread-home-1pt5`

### Spread Market Logic (Critical)

Polymarket uses **different slug prefixes** depending on which team is favored:

| Scenario | Slug Format | Example |
|----------|-------------|---------|
| Home team favored | `spread-home-{value}` | Denver (home) -12.5 → `spread-home-12pt5` |
| Away team favored | `spread-away-{value}` | OKC (away) -8.5 → `spread-away-8pt5` |

**How to determine which to use:**

1. Parse Kalshi event ticker to get `team1` (away) and `team2` (home)
2. Extract team code from market suffix (e.g., `DEN12` → `DEN`)
3. If suffix team == `team1` (away) → use `spread-away-{value}`
4. If suffix team == `team2` (home) → use `spread-home-{value}`

**Code location:** `discovery.rs` → `build_poly_slug()` function

### Why Matches Fail

Even with correct slug format, matches can fail because:

1. **Spread value mismatch:** Kalshi offers many spreads (3, 6, 9, 12, 15, 18, 21, 24, 27), Polymarket offers 2-4 values around the actual line
2. **No underdog spreads:** Polymarket only creates spread markets for the FAVORED team
3. **Date timezone issues:** Polymarket may use different timezone, code tries next-day slug as fallback

### Gamma API for Verification

Query Polymarket's Gamma API to verify market existence:

```bash
# Single market lookup
curl "https://gamma-api.polymarket.com/markets?slug=nba-was-den-2026-01-17-spread-home-12pt5"

# Batch lookup (multiple slugs)
curl "https://gamma-api.polymarket.com/markets?slug=slug1&slug=slug2&slug=slug3"

# All markets for an event
curl "https://gamma-api.polymarket.com/events?slug=nba-was-den-2026-01-17" | jq '.[].markets[].slug'
```

### Debugging Market Pairing

```bash
# Run with pairing debug output
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 DRY_RUN=1 cargo run -p controller --release -- \
  --leagues nba --pairing-debug --pairing-debug-limit 50 --market-type spread
```

This shows:
- Generated Polymarket slugs for each Kalshi market
- Which lookups succeeded/failed
- Match statistics (total, matched, misses by reason)

For detailed debugging of league-specific pairing issues, see `controller/docs/PAIRING_DEBUG_PLAYBOOK.md`.
