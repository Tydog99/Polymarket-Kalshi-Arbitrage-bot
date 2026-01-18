# Remote Trader (`remote-trader`)

This is an **optional** companion binary that connects to a controller/host over WebSocket and executes trades on demand.

## Build

```bash
cargo build -p remote-trader --release
```

## Configure

The trader auto-loads `.env` from the **repo root** (one folder above `trader/`) or from the current directory.

### Controller address (where to connect)

Set `WEBSOCKET_URL` to the controller’s reachable address:

- **Same machine**: `ws://127.0.0.1:9001`
- **Two machines (LAN/WAN)**: `ws://<CONTROLLER_IP>:9001`

### Environment variables

| Variable | Required | Example | Description |
|----------|----------|---------|-------------|
| `WEBSOCKET_URL` | Yes | `ws://192.168.1.10:9001` | Where to connect (controller host + port). |
| `DRY_RUN` | No | `1` | If `1`, the trader will **log the trades it would place** but will not execute real orders. |
| `ONE_SHOT` | No | `1` | If `1`, exit after receiving and handling the first `execute` message (useful for smoke tests). |
| `RUST_LOG` | No | `info` | Logging verbosity. |

### Optional credentials (depending on which platforms you enable)

- **Kalshi**
  - `KALSHI_API_KEY`
  - `KALSHI_PRIVATE_KEY` (PEM contents, not a path)
- **Polymarket**
  - `POLY_PRIVATE_KEY`
  - `POLYMARKET_FUNDER` (or `POLY_FUNDER`)
  - `POLY_SIGNATURE_TYPE` *(optional; default `0`)*
  - `POLYMARKET_API_KEY` *(optional; will be derived if omitted)*
  - `POLYMARKET_API_SECRET` *(optional; will be derived if omitted)*
  - `POLYMARKET_API_PASSPHRASE` *(optional; will be derived if omitted)*

## Run examples

**Trader (client) → connect to controller:**

```bash
DRY_RUN=1 WEBSOCKET_URL=ws://<CONTROLLER_IP>:9001 RUST_LOG=info cargo run -p remote-trader --release
```

**Smoke test mode (exit after first execute):**

```bash
DRY_RUN=1 ONE_SHOT=1 WEBSOCKET_URL=ws://127.0.0.1:9001 RUST_LOG=info cargo run -p remote-trader
```

## Manual trade mode (no controller)

This mode is for validating that the **Polymarket execution path works end-to-end** by simulating the same message handling path the controller uses (`Init` → `ExecuteLeg`).

**Requirements (Polymarket):**
- `TRADER_PLATFORM=polymarket`
- `POLY_PRIVATE_KEY`
- `POLYMARKET_FUNDER` (or `POLY_FUNDER`)

If your **funder** address (from the Polymarket UI) is **different** from the address derived from your private key (the signer), set `POLY_SIGNATURE_TYPE` to match your account type:
- `0` = EOA (signer == funder)
- `1`/`2` = proxy/delegated (signer != funder)

The trader will auto-derive `POLYMARKET_API_KEY` / `POLYMARKET_API_SECRET` / `POLYMARKET_API_PASSPHRASE` from your wallet if you don’t provide them.

If you see an error like `derive-api-key failed: 400 Bad Request {"error":"Could not derive api key!"}`, then Polymarket did not allow auto-derivation for that wallet/account. In that case, you must set:
- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`

### Manual trade (step-by-step)

1. **Pick the right token id**
   - Find the market’s `clobTokenIds` (see section below).
   - Choose the token that corresponds to the outcome you want (based on the `outcomes` array order).

2. **Set env vars**
   - Start with `DRY_RUN=1` to verify logs, then switch to `DRY_RUN=0` to place a real order.
   - If PowerShell has `DRY_RUN` set, it can override `.env`. Unset with `Remove-Item Env:DRY_RUN`.

3. **Run `manual-trade`**
   - `--limit-price-cents` must be an **integer** number of cents (e.g. `63` = $0.63). Decimals like `99.8` are not accepted.
   - You must pass **either** `--contracts` or `--spend-usd`.

**PowerShell example (live):**

```powershell
$env:TRADER_PLATFORM="polymarket"
$env:DRY_RUN="0"
$env:RUST_LOG="info"

# Optional: only needed if signer != funder (proxy/delegated accounts)
$env:POLY_SIGNATURE_TYPE="2"

cargo run -p remote-trader --release -- manual-trade `
  --poly-token <CLOB_TOKEN_ID> `
  --side yes `
  --limit-price-cents 63 `
  --spend-usd 2
```

**Example (dry run):**

```bash
TRADER_PLATFORM=polymarket DRY_RUN=1 RUST_LOG=info \
cargo run -p remote-trader --release -- manual-trade \
  --poly-token <CLOB_TOKEN_ID> \
  --side yes \
  --limit-price-cents 60 \
  --contracts 10
```

**Example (live):**

```bash
TRADER_PLATFORM=polymarket DRY_RUN=0 RUST_LOG=info \
cargo run -p remote-trader --release -- manual-trade \
  --poly-token <CLOB_TOKEN_ID> \
  --side yes \
  --limit-price-cents 60 \
  --spend-usd 5
```

### How to get the Polymarket CLOB token id (`clobTokenIds`)

`--poly-token` expects Polymarket’s **CLOB `tokenId`** (a **decimal** string like `"123456..."`, **not** `0x...`).

**Method A: Polymarket URL slug → Gamma API**

1. Open the market on Polymarket and copy the **slug** from the URL (the last part).
2. Open (replace `<SLUG>`):
   - `https://gamma-api.polymarket.com/markets?slug=<SLUG>`
3. In the JSON response, find **`clobTokenIds`** (and `outcomes`).
   - `clobTokenIds` is a JSON string that contains an array, e.g. `["123...","456..."]`.
4. Use the token that matches the outcome you want (check the `outcomes` array order).

**Method B: Browser DevTools**

1. Open the market page.
2. Press **F12** → **Network** tab → refresh.
3. Filter for `gamma` / `markets`, open the request to `gamma-api.polymarket.com`.
4. In the **Response/Preview**, search for **`clobTokenIds`** and copy the token you need.
