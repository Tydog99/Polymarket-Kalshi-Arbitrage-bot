# NCAA Men's Basketball Support

## Overview

Add support for NCAA Men's Basketball (NCAAMB) arbitrage between Kalshi and Polymarket.

## Platform Details

**Kalshi:**
- Series: `KXNCAAMBGAME`, `KXNCAAMBSPREAD`, `KXNCAAMBTOTAL`
- Ticker format: `KXNCAAMBGAME-26JAN24ILLPUR-ILL`
- Team codes: 2-5 uppercase letters (e.g., `ILL`, `PUR`, `DUKE`, `UCLA`)

**Polymarket:**
- Prefix: `cbb` (college basketball)
- Slug format: `cbb-{team1}-{team2}-{YYYY-MM-DD}`
- Team codes: Lowercase, variable length (e.g., `ill`, `pur`, `duke`, `hawaii`)

## Team Code Mapping

Most codes are identity mappings (Kalshi code lowercased = Polymarket code):
- `ILL` → `ill`, `PUR` → `pur`, `DUKE` → `duke`, `UCLA` → `ucla`

Some require explicit mappings:
- `HAW` → `hawaii` (Hawaii)
- `CSB` → `csu` (CSU Bakersfield)
- `ULM` → `lamon` (Louisiana-Monroe)
- `APP` → `applst` (Appalachian State)

## Implementation

### 1. LeagueConfig Addition
Add `ncaamb` to `config.rs` league configurations.

### 2. Team Cache Seed Script
Create `generate_ncaamb_cache.rs` binary that:
- Fetches all Kalshi NCAAMB events
- Probes Polymarket with multiple slug patterns
- Extracts and saves non-identity mappings

### 3. Discovery-Time Fallback
Modify `discovery.rs` to try alternate slug patterns when primary lookup fails.

## Testing

```bash
# Run seed script to build initial mappings
cargo run --bin generate-ncaamb-cache

# Test discovery
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 cargo run -p controller --release -- --leagues ncaamb
```
