# League-Specific Pairing Analysis Playbook

When market pairing is broken for a specific league, follow this systematic process.

## Step 1: Run Discovery with Debug Output

```bash
# Test a specific league and market type
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 DRY_RUN=1 cargo run -p controller --release -- \
  --leagues {league} --pairing-debug --pairing-debug-limit 50 --market-type {type}

# Example for EPL BTTS:
DISCOVERY_ONLY=1 FORCE_DISCOVERY=1 DRY_RUN=1 cargo run -p controller --release -- \
  --leagues epl --pairing-debug --pairing-debug-limit 50 --market-type btts
```

## Step 2: Compare Generated Slugs vs Actual Polymarket Slugs

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

## Step 3: Diagnose Root Cause

### If wrong team codes are being used:

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

### If ticker parsing is wrong:

Check the parsed output in debug logs:
```
parsed=(26JAN17,CA,RNJ)  # WRONG - should be (CAR,NJ)
parsed=(26JAN17,CAR,NJ)  # CORRECT
```

This indicates issues in `split_team_codes()` in `discovery.rs`.

## Step 4: Fix Cache Issues

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

## Step 5: Fix Ticker Parsing Issues

If the issue is in `split_team_codes()`:
1. Check `is_likely_two_letter_code()` for 2-letter prefixes
2. Check `is_known_two_letter_suffix()` for 2-letter suffixes
3. Update the appropriate match arm in `split_team_codes()` for the string length

Example: NHL uses 2-letter codes (SJ, NJ, LA, TB) that need special handling.

## Step 6: Verify Fix

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

## Reference: Series IDs

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

## Reference: Known Code Differences

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
