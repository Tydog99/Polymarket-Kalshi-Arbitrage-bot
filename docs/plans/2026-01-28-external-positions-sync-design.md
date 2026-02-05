# External Positions Sync (Kalshi + Polymarket) Design

**Date:** 2026-01-28  
**Status:** Draft  
**Goal:** Fetch authoritative account positions from Kalshi + Polymarket directly (no controller stream dependency) and expose them to the local web UI for reconciliation vs `positions.json`.

## Problem

Today the repo maintains an internal ledger (`positions.json`) derived from attempted executions/fills. This is useful for audit and strategy logic, but it is not an authoritative view of actual exchange holdings:

- Positions can exist that were created outside this bot (manual trades, other runs, other systems).
- Network/API failures can leave the internal ledger out of sync with exchange truth.
- For UI and safety, we want to display **actual holdings** and compare them to internal state.

## Overview

Extend the existing Rust `monitor` process to act as a **read-only middleware** that:

1. Loads credentials from `.env` (server-side only).
2. Queries authoritative holdings from:
   - **Kalshi** via existing Rust signed client (`trading/src/kalshi/client.rs` → `get_positions()`).
   - **Polymarket** via the public Data API positions endpoint (`GET https://data-api.polymarket.com/positions?user=...`).
3. Returns a **single combined response** to the UI containing:
   - internal `positions.json`
   - external holdings snapshot
   - reconciliation results
4. Optionally exposes an external-only endpoint for debugging.

## Non-Goals

- Running execution/trading from the web UI.
- Pushing secrets to the browser.
- Rebuilding the internal ledger from full order history (expensive + not required for “current truth”).

## Design

### Security model (critical)

- **Secrets never leave the server process.**
  - Private keys and signing material remain in the Rust `monitor` process.
  - Browser receives only read-only JSON responses (positions, diffs) and optional non-sensitive “public config”.

### Why `external_ids` in `positions.json`

External holdings endpoints are keyed by **market/contract/token identifiers**, not by `order_id`.

- Polymarket Data API positions are keyed by `asset` (and also include `conditionId` + metadata).
- Kalshi positions are keyed by stable market/contract identifiers returned by the portfolio endpoints.

Therefore, the internal `ArbPosition` needs optional identifiers that allow deterministic join:

- `kalshi_market_ticker` (or the exact stable identifier returned by Kalshi portfolio endpoints)
- Polymarket join keys (pick what we have available, ordered by preference):
  - `poly_asset_yes`, `poly_asset_no` (Data API `asset`)
  - `poly_condition_id` (Data API `conditionId`, useful for grouping/filtering)
  - (optional) `poly_token_id_yes`, `poly_token_id_no` if we later add CLOB holdings support
- `poly_market_slug` (debug/UI only)
- `account_ids` (optional): `poly_address`, `kalshi_member_id` (debug/UI only)

### Components

#### A) Internal ledger (Rust)

- Extend `controller::position_tracker::ArbPosition` with optional `external_ids`.
- Populate these fields when the bot has the information (during discovery/execution), keeping backward compatibility for old records.

#### B) External holdings fetch (Rust `monitor`)

Single responsibility: fetch & normalize authoritative holdings server-side.

- Kalshi: reuse `trading` crate client `KalshiApiClient::get_positions()`.
- Polymarket: call Data API `GET /positions?user=...` and normalize response.

#### C) Reconciler (Rust `monitor`)

Reads:
- internal ledger: `positions.json`
- external snapshot: Kalshi + Polymarket (fetched by `monitor`)

Outputs:
- reconciliation object embedded in the API response (per-market diffs + status)

#### D) UI integration (monitor)

Primary model: **page-load + refresh button + optional polling** against a single endpoint.

Endpoints:

- **Primary**: `GET /api/positions/overview?refresh_external=0|1` → returns `{ internal, external, reconciliation }`
- **Debug**: `GET /api/positions/external?refresh=0|1` → returns `{ external }` only

## Data Contracts

### 1) `positions.json` schema extension (backward compatible)

Add to each `ArbPosition`:

- `external_ids?: { ... }`
- `external_ids.kalshi_market_ticker?: string`
- `external_ids.poly_asset_yes?: string`
- `external_ids.poly_asset_no?: string`
- `external_ids.poly_condition_id?: string`
- (optional) `external_ids.poly_token_id_yes?: string`
- (optional) `external_ids.poly_token_id_no?: string`
- `external_ids.poly_market_slug?: string`
- `external_ids.account_ids?: { poly_address?: string, kalshi_member_id?: string }`

Backward compatibility requirements:
- `external_ids` must be `Option<>` / `#[serde(default)]`
- Old files must load without changes

### 2) External snapshot schema

`external` object in API responses:

- `snapshot_time`: ISO-8601
- `kalshi`: normalized from `KalshiPositionsResponse` (`trading/src/kalshi/types.rs`)
- `polymarket`: normalized from Polymarket Data API positions response
- `errors`: optional `{ kalshi?: string, polymarket?: string }`

Normalized position entries should be lossless enough to debug:
- Kalshi normalized: `{ market_ticker, side, contracts, avg_price?, cost_basis?, ... }`
- Polymarket normalized (Data API): `{ proxy_wallet, asset, condition_id, size, avg_price, current_value, cash_pnl, percent_pnl, slug, outcome, ... }`

### 3) Reconciliation schema

`reconciliation` object in the overview API response:

- `snapshot_time`
- `summary`: counts of `match | mismatch | unknown_mapping | api_error`
- `by_market_id`: map keyed by internal `market_id` with:
  - internal legs (`kalshi_yes/no`, `poly_yes/no`)
  - external legs (mapped holdings)
  - diffs (`diff_contracts`, `diff_cost_basis?`)
  - status + reason

## Caching / refresh strategy

- Use a short TTL cache in the Rust `monitor` process (e.g., 5–30 seconds) to avoid hammering APIs.
- Support explicit cache bypass:
  - `/api/positions/overview?refresh_external=1`
  - `/api/positions/external?refresh=1`
- (Optional) store last-good snapshot in memory only (initial version). Disk persistence is optional later.

## Parallel Implementation Plan

### Workstream A — Rust schema + identifiers

**Deliverable:** `positions.json` can include `external_ids` for new positions.

- Extend `ArbPosition` with optional `external_ids`
- Populate:
  - Kalshi stable market/contract identifier used by portfolio endpoints
  - Polymarket join keys (`asset` and/or `conditionId`) sufficient to reconcile via Data API
- Ensure backward compatible serde defaults

### Workstream B — `monitor`: external holdings fetch (Kalshi + Polymarket)

**Deliverable:** new debug endpoint `GET /api/positions/external`.

- Kalshi:
  - load env credentials
  - call `KalshiApiClient::get_positions()`
  - normalize
- Polymarket:
  - determine `user` address (default from `POLY_FUNDER`, allow override)
  - call Data API `/positions`
  - normalize

### Workstream C — `monitor`: overview endpoint + reconciliation

**Deliverable:** primary endpoint `GET /api/positions/overview`.

- Load internal positions by reading/parsing `positions.json`
- Fetch external (or reuse cached external snapshot)
- Reconcile internal vs external and return a single combined response

### Workstream D — UI integration

**Deliverable:** UI shows Internal vs External vs Reconciled.

- Add “Refresh” button and optional 30s auto-refresh toggle that calls `/api/positions/overview`.
- (Optional) add a debug panel that calls `/api/positions/external` and shows raw provider payloads/errors.

## Environment variables

Use existing `.env` credentials; do not introduce new secret-bearing vars.

Optional (non-secret or convenience):
- `POLY_POSITIONS_USER` (override address used for Polymarket Data API; default = `POLY_FUNDER`)

UI-safe (optional):
- `PUBLIC_POLY_ADDRESS` (or derived from `POLY_FUNDER`)
- `PUBLIC_ENV` (e.g., `dev`, `paper`, `live`)

## Acceptance Criteria

- External snapshots fetch successfully for both platforms with current `.env`.
- Reconciliation flags mismatches and “unknown mapping” deterministically.
- Browser never receives private keys / API secrets.
- Old `positions.json` loads and renders unchanged.

