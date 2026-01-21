# Monitoring Web App - History Endpoints (No SQLite) Implementation Plan

> **Goal:** Enable the History page to fetch **orders + fills/trades** directly from **Kalshi** and **Polymarket CLOB** on page load (server-side), without SQLite.  
> **Key constraint:** **Never expose credentials to the browser**. All history fetch happens inside the monitor server using `.env`.

**Architecture:**  
UI → `monitor_server` → (Kalshi Trade API v2 + Polymarket CLOB API) → normalized “fills/bets by market” response → UI.

**Tech stack:** Rust (`trading` crate clients + new `monitor_server` axum API), `serde` for types, in-memory TTL cache in monitor server.

---

## Parallel Workstreams (recommended)

To maximize parallelism and avoid merge conflicts:

- **Agent K (Kalshi)** works only in `trading/src/kalshi/*` (+ new fixtures/tests under `trading/tests/*`).
- **Agent P (Polymarket)** works only in `trading/src/polymarket/*` (+ new fixtures/tests under `trading/tests/*`).
- Each agent uses a separate branch/worktree and ships a PR.

---

## Workstream K (Kalshi) — Add Orders + Fills Listing

Kalshi endpoints (Trade API v2):
- **Orders:** `GET /trade-api/v2/portfolio/orders`
- **Fills:** `GET /trade-api/v2/portfolio/fills` (pagination via `cursor`)

### Task K1: Add Kalshi history types (orders + fills)

**Files:**
- Modify: `trading/src/kalshi/types.rs`

**Step 1: Add “list orders” response types**

Add structs to deserialize `GET /portfolio/orders` response, including:
- `KalshiOrdersResponse { orders: Vec<KalshiOrder>, cursor: Option<String> }`
- `KalshiOrder` fields needed for history:
  - `order_id`, `client_order_id`, `ticker`, `side`, `action`, `status`
  - `yes_price`/`no_price` (as present)
  - `taker_fill_count`, `maker_fill_count`, `created_time`, `remaining_count`

**Step 2: Add “list fills” response types**

Add structs to deserialize `GET /portfolio/fills` response, including:
- `KalshiFillsResponse { fills: Vec<KalshiFill>, cursor: Option<String> }`
- `KalshiFill` fields needed for history:
  - `fill_id`, `trade_id`, `order_id`, `client_order_id`
  - `ticker` (or `market_ticker` if present), `side`, `action`
  - `count` and/or `count_fp`
  - `price` and/or `yes_price`/`no_price`
  - `created_time`, `ts`
  - `is_taker`

**Step 3: Add lightweight unit tests (deserialize)**

Create JSON fixtures and unit tests (see Task K3) to validate these structs deserialize correctly.

---

### Task K2: Add KalshiApiClient methods for orders + fills

**Files:**
- Modify: `trading/src/kalshi/client.rs`

**Step 1: Add query-parameter builders (pure functions)**

Add small helpers for query strings to keep logic testable:
- `build_orders_query(params: &KalshiOrdersQuery) -> String`
- `build_fills_query(params: &KalshiFillsQuery) -> String`

Where query structs include:
- `ticker: Option<String>`
- `order_id: Option<String>` (fills)
- `status: Option<String>` (orders, if supported)
- `min_ts: Option<i64>`, `max_ts: Option<i64>` (fills)
- `limit: Option<u16>` (default 100, cap 200 per docs)
- `cursor: Option<String>`

**Step 2: Add `list_orders()`**

Implement:
- `pub async fn list_orders(&self, q: &KalshiOrdersQuery) -> Result<KalshiOrdersResponse>`

Use authenticated GET (`self.get`) with:
- path: `"/portfolio/orders" + query`
- handle cursor pagination in caller (don’t auto-loop here).

**Step 3: Add `list_fills()`**

Implement:
- `pub async fn list_fills(&self, q: &KalshiFillsQuery) -> Result<KalshiFillsResponse>`

Use authenticated GET with:
- path: `"/portfolio/fills" + query`

**Step 4: Add pagination helpers (optional but recommended)**

To simplify monitor server usage, add:
- `pub async fn list_all_fills_since(&self, min_ts: i64, limit_pages: usize) -> Result<Vec<KalshiFill>>`

This method should:
- loop on `cursor` up to `limit_pages` (safety)
- stop when cursor is empty / null
- respect existing rate-limit behavior in `get()`

**Step 5: Verify**

Run:
- `cargo test -p trading kalshi`
- `cargo check -p trading`

---

### Task K3: Add Kalshi fixtures + tests (no live credentials)

**Files:**
- Create: `trading/tests/fixtures/kalshi_orders.json`
- Create: `trading/tests/fixtures/kalshi_fills.json`
- Create: `trading/tests/kalshi_history_types_tests.rs`

**Step 1: Add fixture JSON**

Add minimal but realistic JSON payloads for:
- orders list response (`orders`, `cursor`)
- fills list response (`fills`, `cursor`)

**Step 2: Add tests**

Tests should:
- read fixture JSON from disk
- `serde_json::from_str` into the new response structs
- assert key fields exist and parse

**Step 3: Verify**

Run:
- `cargo test -p trading kalshi_history_types_tests`

---

## Workstream P (Polymarket) — Add Open Orders + Trade History

Polymarket CLOB endpoints:
- **Open orders:** `GET /data/orders` (L2 auth)
- **Trade history:** `GET /data/trades` (L2 auth)

### Task P1: Add Polymarket history types (open orders + trades)

**Files:**
- Modify: `trading/src/polymarket/types.rs`

**Step 1: Add open order type**

Add `PolyOpenOrder` to match `GET /data/orders` entries (fields commonly present):
- `id`, `status`, `market`, `asset_id`, `outcome`
- `price`, `side`, `original_size`, `size_matched`
- `maker_address`, `owner`, `created_at`, `expiration`
- `associate_trades` (array; may be empty)

**Step 2: Add trade type**

Add `PolyTrade` to match `GET /data/trades` entries:
- `id`, `taker_order_id`, `market`, `asset_id`, `outcome`
- `side`, `size`, `price`, `fee_rate_bps`
- `status`, `match_time`, `last_update`, `transaction_hash`, `bucket_index`
- `maker_address`, `owner`, `type`
- `maker_orders: Vec<PolyMakerOrder>`

Add `PolyMakerOrder` with:
- `order_id`, `maker_address`, `owner`, `matched_amount`, `fee_rate_bps`
- `price`, `asset_id`, `outcome`, `side`

**Step 3: Add deserialize tests (see P3)**

---

### Task P2: Add PolymarketAsyncClient methods for open orders + trades

**Files:**
- Modify: `trading/src/polymarket/client.rs`

**Step 1: Add a generic L2-authenticated GET helper**

Add a method to reduce duplication:
- `async fn get_l2_json<T: DeserializeOwned>(&self, path_with_query: &str, creds: &PreparedCreds) -> Result<T>`

This should:
- build headers via existing `build_l2_headers("GET", path, None, creds)`
- execute request, validate status, parse JSON

**Step 2: Implement `get_open_orders()`**

Implement:
- `pub async fn get_open_orders(&self, market: Option<&str>, asset_id: Option<&str>) -> Result<Vec<PolyOpenOrder>>`

Use:
- path: `"/data/orders"` with optional query params `market` and/or `asset_id`

**Step 3: Implement `get_trades()` (history)**

Implement:
- `pub async fn get_trades(&self, q: &PolyTradesQuery) -> Result<Vec<PolyTrade>>`

Where `PolyTradesQuery` supports:
- `maker: Option<String>` and/or `taker: Option<String>` (use funder/wallet as appropriate)
- `market: Option<String>`
- `after: Option<i64>`, `before: Option<i64>` (Unix seconds per docs)
- `id: Option<String>`

**Step 4: Add safety for large responses**

Add guardrails in callers (monitor server), but optionally add:
- `max_items` filtering after parse
- request timeouts already exist on the underlying client

**Step 5: Verify**

Run:
- `cargo test -p trading polymarket`
- `cargo check -p trading`

---

### Task P3: Add Polymarket fixtures + tests (no live credentials)

**Files:**
- Create: `trading/tests/fixtures/polymarket_orders.json`
- Create: `trading/tests/fixtures/polymarket_trades.json`
- Create: `trading/tests/polymarket_history_types_tests.rs`

**Step 1: Add fixture JSON**

Add minimal but realistic JSON payloads for:
- `/data/orders` response (array of open orders)
- `/data/trades` response (array of trades with at least one `maker_orders` entry)

**Step 2: Add tests**

Tests should:
- read fixture JSON from disk
- deserialize into `Vec<PolyOpenOrder>` / `Vec<PolyTrade>`
- assert key fields parse and optional fields behave

**Step 3: Verify**

Run:
- `cargo test -p trading polymarket_history_types_tests`

---

## Integration Notes (for the monitor server)

After Workstreams K + P land, the monitor server can implement History without SQLite by:

- Fetching Kalshi fills via `list_all_fills_since(...)` (cursor pagination).
- Fetching Polymarket trades via `get_trades(...)` filtered by `maker`/`taker` and `after/before`.
- Normalizing into a shared struct:
  - `platform`, `market_key` (join via `.discovery_cache.json`), `side`, `price`, `size`, `ts`, `fees`, `order_id`
- Returning a computed “bets by market” view + all-time W/L/P&L approximation.
- Adding **in-memory TTL cache** (60–300s) + `?refresh=1` bypass.

