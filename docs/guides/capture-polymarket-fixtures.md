# Capturing Polymarket API Fixtures

This guide walks through capturing real Polymarket API traffic for replay testing.

## Background

The capture/replay system records real HTTP exchanges so we can replay them in tests. This ensures our order execution code handles real API responses correctly.

**What's already done:**
- Capture middleware built (`trading/src/capture.rs`)
- Manual trade CLI supports Polymarket (`cargo run -p remote-trader -- manual-trade`)
- Kalshi fixtures captured (see `controller/tests/integration/fixtures/kalshi_*.json`)

**What's needed:**
- Real Polymarket API captures for: full fill, partial fill, no fill, errors

## Prerequisites

### Environment Variables

Ensure these are set (in `.env` or exported):

```bash
POLY_PRIVATE_KEY=0x...          # Your wallet private key
POLY_FUNDER=0x...               # Your funder address (from Polymarket UI)

# Optional - only if signer != funder (proxy/delegated accounts):
POLY_SIGNATURE_TYPE=2           # 0=EOA, 1/2=proxy/delegated
```

### Find a Polymarket Token ID

Markets have two token IDs (YES and NO outcomes):

```bash
# Get token IDs for a market by slug
curl -s "https://gamma-api.polymarket.com/markets?slug=nba-bos-nyk-2026-01-20" | jq '.[0] | {slug, clobTokenIds, outcomes, outcomePrices}'
```

The `clobTokenIds` array corresponds to the `outcomes` array (first token = first outcome).

### Check Order Book Liquidity

Before placing orders, check available liquidity to avoid unexpected fills:

```bash
# Use the Polymarket CLOB API or check the UI
# The UI shows the order book depth at polymarket.com
```

## Capture Commands

### Base Command Format

```bash
CAPTURE_DIR=./.captures TRADER_PLATFORM=polymarket DRY_RUN=0 \
  dotenvx run -- cargo run -p remote-trader --release -- manual-trade \
  --poly-token <TOKEN_ID> \
  --side <yes|no> \
  --limit-price-cents <PRICE> \
  --contracts <COUNT>
```

### Dry Run First

Always test with `DRY_RUN=1` first to verify the command works:

```bash
CAPTURE_DIR=./.captures TRADER_PLATFORM=polymarket DRY_RUN=1 \
  dotenvx run -- cargo run -p remote-trader --release -- manual-trade \
  --poly-token 12345678901234567890 \
  --side yes \
  --limit-price-cents 55 \
  --contracts 1
```

Note: Dry run won't capture anything (no real HTTP requests made).

## Scenarios to Capture

| Scenario | How to trigger | Expected response |
|----------|----------------|-------------------|
| **Full fill** | Small order at/above market price | Order fills completely |
| **Partial fill** | Large order exceeding available liquidity | Order partially fills |
| **No fill** | Order at very stale price (e.g., 1¢ for a 50¢ market) | Order cancels with 0 fill |
| **Insufficient balance** | Order exceeding wallet balance | Error response |
| **Invalid token** | Use a fake/malformed token ID | Error response |

## Step-by-Step Capture Session

### 1. Find a Market

```bash
# Find active markets
curl -s "https://gamma-api.polymarket.com/markets?active=true&limit=10" | \
  jq '.[] | {slug, clobTokenIds, outcomePrices}'
```

Pick a market with reasonable liquidity but not too deep (so partial fills are possible).

### 2. Capture Full Fill

Place a small order at market price:

```bash
CAPTURE_DIR=./.captures TRADER_PLATFORM=polymarket DRY_RUN=0 \
  dotenvx run -- cargo run -p remote-trader --release -- manual-trade \
  --poly-token <YOUR_TOKEN_ID> \
  --side yes \
  --limit-price-cents 55 \
  --contracts 5
```

### 3. Capture No Fill

Place an order at a price that won't match:

```bash
CAPTURE_DIR=./.captures TRADER_PLATFORM=polymarket DRY_RUN=0 \
  dotenvx run -- cargo run -p remote-trader --release -- manual-trade \
  --poly-token <YOUR_TOKEN_ID> \
  --side yes \
  --limit-price-cents 1 \
  --contracts 5
```

### 4. Capture Partial Fill

Find a market with limited liquidity at a price level, then order more than available:

```bash
CAPTURE_DIR=./.captures TRADER_PLATFORM=polymarket DRY_RUN=0 \
  dotenvx run -- cargo run -p remote-trader --release -- manual-trade \
  --poly-token <YOUR_TOKEN_ID> \
  --side yes \
  --limit-price-cents 50 \
  --contracts 500
```

### 5. Capture Error Responses

**Invalid token:**
```bash
CAPTURE_DIR=./.captures TRADER_PLATFORM=polymarket DRY_RUN=0 \
  dotenvx run -- cargo run -p remote-trader --release -- manual-trade \
  --poly-token 99999999999999999999 \
  --side yes \
  --limit-price-cents 50 \
  --contracts 1
```

**Insufficient balance** (order more than your wallet holds):
```bash
CAPTURE_DIR=./.captures TRADER_PLATFORM=polymarket DRY_RUN=0 \
  dotenvx run -- cargo run -p remote-trader --release -- manual-trade \
  --poly-token <YOUR_TOKEN_ID> \
  --side yes \
  --limit-price-cents 99 \
  --contracts 10000
```

## After Capturing

### 1. Check Your Captures

```bash
# List all capture sessions
ls -la ./.captures/

# Check a session's manifest
cat ./.captures/session_YYYY-MM-DD_HH-MM-SS/manifest.json

# View a captured exchange
cat ./.captures/session_YYYY-MM-DD_HH-MM-SS/001_POST_poly_order.json | jq .
```

### 2. Identify Each Capture

Review each session and note what scenario it represents:

```bash
# Quick check of response status/outcome
for dir in ./.captures/session_*; do
  echo "=== $dir ==="
  cat "$dir"/*.json 2>/dev/null | jq -r '.response.status, .response.body_parsed.status // .response.body_parsed.error // "unknown"' | head -2
done
```

### 3. Copy to Test Fixtures

Copy the captures to the test fixtures directory with descriptive names:

```bash
# Full fill
cp ./.captures/session_XXXX/001_POST_poly_order.json \
   ./controller/tests/integration/fixtures/poly_full_fill_real.json

# No fill
cp ./.captures/session_YYYY/001_POST_poly_order.json \
   ./controller/tests/integration/fixtures/poly_no_fill_real.json

# Partial fill
cp ./.captures/session_ZZZZ/001_POST_poly_order.json \
   ./controller/tests/integration/fixtures/poly_partial_fill_real.json

# Error responses
cp ./.captures/session_AAAA/001_POST_poly_order.json \
   ./controller/tests/integration/fixtures/poly_invalid_token.json
```

### 4. Commit the Fixtures

```bash
git add controller/tests/integration/fixtures/poly_*.json
git add captures/  # Optional: include raw captures for reference

git commit -m "feat(fixtures): add real Polymarket API captures for replay testing

Captured from live Polymarket API:
- poly_full_fill_real.json
- poly_no_fill_real.json
- poly_partial_fill_real.json
- poly_invalid_token.json

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>"
```

## Fixture Format Reference

Captured files follow this structure:

```json
{
  "captured_at": "2026-01-20T...",
  "sequence": 1,
  "latency_ms": 142,
  "request": {
    "method": "POST",
    "url": "https://clob.polymarket.com/order",
    "headers": { "content-type": "application/json" },
    "body": { ... }
  },
  "response": {
    "status": 200,
    "headers": { ... },
    "body_raw": "...",
    "body_parsed": { ... }
  }
}
```

## Budget

Expect to spend $20-50 on test orders across all scenarios.

## Troubleshooting

### "Missing POLY_PRIVATE_KEY"
Ensure your `.env` has `POLY_PRIVATE_KEY` set (the actual key, not a path).

### "Could not derive api key"
Your wallet may not support auto-derivation. Manually set:
- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`

### Captures directory is empty
Dry run mode (`DRY_RUN=1`) doesn't make real HTTP requests. Use `DRY_RUN=0`.

### Order fills unexpectedly
Check the order book liquidity before placing orders. Use the Polymarket UI to see depth.
