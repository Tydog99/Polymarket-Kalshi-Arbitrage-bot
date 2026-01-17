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
```

## Architecture Overview

This is a Rust arbitrage bot that monitors price discrepancies between Kalshi and Polymarket prediction markets. The core principle: in prediction markets, YES + NO = $1.00. Arbitrage exists when buying YES on one platform and NO on another costs less than $1.00.

### Data Flow

```
WebSocket Feeds (kalshi.rs, polymarket.rs)
    ↓
Global State with Lock-Free Orderbook Cache (types.rs)
    ↓
Heartbeat Arbitrage Detection (main.rs, every 60s)
    ↓
Execution Loop (execution.rs)
    ↓
Platform Orders (kalshi.rs, polymarket_clob.rs)
    ↓
Position Tracking (position_tracker.rs)
```

### Key Modules

- **`main.rs`** - Entry point, WebSocket orchestration, heartbeat-based arb detection
- **`types.rs`** - Core data structures including `AtomicOrderbook` (lock-free using packed u64 with CAS loops)
- **`execution.rs`** - Concurrent order execution with in-flight deduplication (8-slot bitmask for 512 markets)
- **`kalshi.rs`** - Kalshi REST/WebSocket client with RSA signature authentication
- **`polymarket.rs`** - Polymarket WebSocket client and Gamma API integration
- **`polymarket_clob.rs`** - Polymarket CLOB execution with EIP-712 and HMAC-SHA256 signing
- **`discovery.rs`** - Cross-platform market matching with persistent caching (2-hour TTL)
- **`circuit_breaker.rs`** - Risk management: position limits, daily loss limits, error tracking, cooldown
- **`position_tracker.rs`** - Fill recording, P&L calculation, state persistence to `positions.json`
- **`cache.rs`** - Team code bidirectional mapping between platforms
- **`config.rs`** - League definitions, API endpoints, thresholds (ARB_THRESHOLD = 0.995)

### Arbitrage Types

| Type | Description |
|------|-------------|
| `poly_yes_kalshi_no` | Buy Polymarket YES + Kalshi NO |
| `kalshi_yes_poly_no` | Buy Kalshi YES + Polymarket NO |
| `poly_only` | Both sides on Polymarket (rare) |
| `kalshi_only` | Both sides on Kalshi (rare) |

### Lock-Free Orderbook Design

`AtomicOrderbook` uses a packed u64 format for cache-line efficiency:
- Bit layout: `[yes_ask:16][no_ask:16][yes_size:16][no_size:16]`
- Updates via compare-and-swap loops
- 64-byte aligned for SIMD compatibility

### Fee Calculation

- **Kalshi**: `ceil(0.07 × contracts × price × (1-price))` - factored into arb detection
- **Polymarket**: Zero trading fees

## Environment Variables

**Required credentials:**
- `KALSHI_API_KEY_ID`, `KALSHI_PRIVATE_KEY_PATH` (PEM file)
- `POLY_PRIVATE_KEY` (0x-prefixed), `POLY_FUNDER` (wallet address)

**Execution:** `DRY_RUN` (default: 1), `RUST_LOG` (default: info)

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

**Circuit breaker:** `CB_ENABLED`, `CB_MAX_POSITION_PER_MARKET`, `CB_MAX_TOTAL_POSITION`, `CB_MAX_DAILY_LOSS`, `CB_MAX_CONSECUTIVE_ERRORS`, `CB_COOLDOWN_SECS`

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

Soccer (EPL, Bundesliga, La Liga, Serie A, Ligue 1, UCL, UEL, EFL Championship), NBA, NFL, NHL, MLB, MLS, NCAAF, Esports (CS2, LoL, CoD)

## Market Pairing: Kalshi ↔ Polymarket Slug Mapping

This section documents how market tickers are constructed on each platform and how to map between them.

### Kalshi Ticker Format

Kalshi uses a structured ticker format:

```
{SERIES}-{DATE}{AWAY}{HOME}-{SUFFIX}
```

**Example:** `KXNBASPREAD-26JAN17WASDEN-DEN12`

| Component | Value | Meaning |
|-----------|-------|---------|
| Series | `KXNBASPREAD` | NBA spread markets |
| Date | `26JAN17` | January 17, 2026 |
| Away Team | `WAS` | Washington Wizards (away) |
| Home Team | `DEN` | Denver Nuggets (home) |
| Suffix | `DEN12` | Denver wins by 12+ points |

**Key insight:** Standard sports convention - **AWAY team listed FIRST, HOME team listed SECOND**.

### Polymarket Slug Format

Polymarket uses URL-friendly slugs:

```
{league}-{away}-{home}-{date}-{market_type}-{qualifier}
```

**Examples:**
- Moneyline: `nba-was-den-2026-01-17`
- Spread (home favored): `nba-was-den-2026-01-17-spread-home-12pt5`
- Spread (away favored): `nba-okc-mia-2026-01-17-spread-away-8pt5`
- Total: `nba-was-den-2026-01-17-total-230pt5`

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

### League-Specific Pairing Analysis Playbook

When market pairing is broken for a specific league, follow this systematic process:

#### Step 1: Run Discovery with Debug Output

```bash
# Test a specific league and market type
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 DRY_RUN=1 cargo run -p controller --release -- \
  --leagues {league} --pairing-debug --pairing-debug-limit 50 --market-type {type}

# Example for EPL BTTS:
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 DRY_RUN=1 cargo run -p controller --release -- \
  --leagues epl --pairing-debug --pairing-debug-limit 50 --market-type btts
```

#### Step 2: Compare Generated Slugs vs Actual Polymarket Slugs

```bash
# Get actual Polymarket slugs for the league
# Series IDs: epl=10188, nhl=10346, nba=10345, nfl=10187, etc.
curl -s "https://gamma-api.polymarket.com/events?series_id={SERIES_ID}&closed=false&limit=30" \
  | jq -r '.[].slug'

# Get specific market type slugs (e.g., btts)
curl -s "https://gamma-api.polymarket.com/events?series_id={SERIES_ID}&closed=false&limit=50" \
  | jq -r '.[].markets[]? | select(.slug | contains("btts")) | .slug'
```

Compare the generated slugs from Step 1 with actual Polymarket slugs. Look for patterns:
- Wrong team codes (e.g., `spurs` vs `tot`, `man_city` vs `mac`)
- Wrong code length (e.g., `manchester_united_fc` vs `mun`)
- Parsing errors (e.g., `ca-rnj` vs `car-nj`)

#### Step 3: Diagnose Root Cause

**If wrong team codes are being used:**

Check the cache for conflicting entries:
```bash
# Check forward mappings (poly_code -> kalshi_code)
grep -E '"epl:(liv|mac|che|tot)"' controller/kalshi_team_cache.json

# Check for identity mappings that override correct mappings
# BAD: "epl:lfc": "lfc" when "epl:liv": "lfc" exists
grep -E '"epl:(lfc|mci|cfc|avl|nfo|whu)"' controller/kalshi_team_cache.json

# Check for long-form aliases causing reverse lookup issues
grep -E '"epl:(spurs|forest|man_utd|wolves|villa)"' controller/kalshi_team_cache.json
```

**Cache problem patterns:**
1. **Identity mappings** like `epl:lfc -> lfc` override correct reverse lookups when `epl:liv -> lfc` exists
2. **Long-form aliases** like `epl:spurs -> tot` create reverse `epl:tot -> spurs` instead of `epl:tot -> tot`

**If ticker parsing is wrong:**

Check the parsed output in debug logs:
```
parsed=(26JAN17,CA,RNJ)  # WRONG - should be (CAR,NJ)
parsed=(26JAN17,CAR,NJ)  # CORRECT
```

This indicates issues in `split_team_codes()` in `discovery.rs`.

#### Step 4: Fix Cache Issues

Remove problematic entries:
```bash
cd controller

# Remove identity mappings that conflict
sed -i '' '/"epl:lfc": "lfc",/d' kalshi_team_cache.json
sed -i '' '/"epl:mci": "mci",/d' kalshi_team_cache.json

# Remove long-form aliases
sed -i '' '/"epl:spurs": "tot",/d' kalshi_team_cache.json
sed -i '' '/"epl:man_city": "mci",/d' kalshi_team_cache.json
```

**Rule: The cache should only contain:**
- Forward mappings where poly_code ≠ kalshi_code (e.g., `epl:liv -> lfc`)
- NO identity mappings for codes that have different poly codes
- NO long-form aliases that map to the same kalshi code as short codes

#### Step 5: Fix Ticker Parsing Issues

If the issue is in `split_team_codes()`:
1. Check `is_likely_two_letter_code()` for 2-letter prefixes
2. Check `is_known_two_letter_suffix()` for 2-letter suffixes
3. Update the appropriate match arm in `split_team_codes()` for the string length

Example: NHL uses 2-letter codes (SJ, NJ, LA, TB) that need special handling.

#### Step 6: Verify Fix

```bash
# Re-run discovery and check match rate
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 DRY_RUN=1 cargo run -p controller --release -- \
  --leagues {league} --pairing-debug --pairing-debug-limit 100 --market-type game 2>&1 | grep "SUMMARY"

# Verify against actual Polymarket market count
curl -s "https://gamma-api.polymarket.com/events?series_id={SERIES_ID}&closed=false&limit=100" \
  | jq -r '.[].markets[]? | select(.slug | contains("{type}") | not) | .slug' | wc -l
```

**Success criteria:** Match rate should equal 100% of available Polymarket markets. Misses should only be for:
- Future dates Polymarket hasn't created yet
- Spread/total values Polymarket doesn't offer

#### Reference: Series IDs

| League | Series ID |
|--------|-----------|
| EPL | 10188 |
| Bundesliga | 10189 |
| La Liga | 10190 |
| Serie A | 10191 |
| Ligue 1 | 10192 |
| UCL | 10193 |
| UEL | 10194 |
| EFL Championship | 10195 |
| NBA | 10345 |
| NFL | 10187 |
| NHL | 10346 |
| MLB | 10347 |
| MLS | 10348 |
| NCAAF | 10349 |

#### Reference: Known Code Differences

| League | Poly Code | Kalshi Code | Team |
|--------|-----------|-------------|------|
| EPL | liv | lfc | Liverpool |
| EPL | mac | mci | Manchester City |
| EPL | che | cfc | Chelsea |
| EPL | ast | avl | Aston Villa |
| EPL | not | nfo | Nottingham Forest |
| EPL | wes | whu | West Ham |
| NHL | lak | la | LA Kings |
| NHL | las | vgk | Vegas Golden Knights |
| NHL | mon | mtl | Montreal Canadiens |
| NHL | cal | cgy | Calgary Flames |
| NHL | utah | uta | Utah Hockey Club |
