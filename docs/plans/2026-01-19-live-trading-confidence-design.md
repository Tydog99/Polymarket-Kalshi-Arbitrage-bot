# Live Trading Confidence Strategy

## Problem

The arbitrage bot will trade real money. Bugs have been found at every stage tested so far (discovery, matching, arb detection, dry-run execution). Live trading has not been tested and likely contains bugs in:

1. Actual trade execution in Kalshi/Polymarket clients
2. Parsing and handling of trade responses
3. Handling partial fills on either side
4. Position tracking over time
5. Bot restart/recovery with open positions

## Solution

A two-phase testing strategy that captures real API traffic and replays it in controlled test scenarios.

## Design

### Capture System

**HTTP middleware** intercepts order-related traffic and writes fixture files:

- Enabled via `CAPTURE_DIR` environment variable
- Filters to only capture order endpoints (`/orders`, `/fills`)
- Works with any entry point: manual trade CLI, full trading sessions, trader binary
- Zero overhead when disabled (middleware not added)

**Capture file format** (one file per request/response):

```json
{
  "captured_at": "2026-01-19T20:15:00.123Z",
  "latency_ms": 142,
  "request": {
    "method": "POST",
    "url": "https://trading-api.kalshi.com/trade-api/v2/portfolio/orders",
    "headers": { "content-type": "application/json" },
    "body": { "ticker": "KXNBA-...", "side": "no", "action": "buy", "count": 5, "no_price": 45 }
  },
  "response": {
    "status": 200,
    "headers": { "content-type": "application/json" },
    "body_raw": "{\"order\": {\"order_id\": \"abc123\", ...}}",
    "body_parsed": { "order": { "order_id": "abc123", "status": "filled", "taker_fill_count": 5 } }
  }
}
```

**Directory structure:**

```
fixtures/
├── session_2026-01-19_live/
│   ├── 001_POST_kalshi_orders.json
│   ├── 002_POST_poly_order.json
│   └── manifest.json
└── manual_partial_fill/
    ├── 001_POST_kalshi_orders.json
    └── manifest.json
```

Files are prefixed with monotonic sequence numbers for unambiguous replay order.

### Replay Test Harness

Tests load fixture files into `wiremock` mock server, then run actual client code against it:

```rust
#[tokio::test]
async fn test_arb_execution_kalshi_partial_fill() {
    let mock_server = MockServer::start().await;

    // Load fixtures in sequence
    mount_fixture(&mock_server, "fixtures/kalshi_full_fill.json").await;
    mount_fixture(&mock_server, "fixtures/poly_partial_fill.json").await;

    // Create client pointing at mock
    let kalshi = KalshiClient::new_with_base_url(mock_server.uri());

    // Run code under test
    let result = execute_leg(&kalshi, &order_req).await;

    // Assert
    assert_eq!(result.filled_qty, 6);
}
```

Scenarios can be composed by loading fixtures from different capture sessions.

### Test Scenarios

**Kalshi (priority):**

| Scenario | How to trigger | What it tests |
|----------|----------------|---------------|
| Full fill | Small order at market price | Happy path, response parsing |
| Partial fill | Larger order at limit price | Partial handling, position tracking |
| No fill | Order at stale price | Cancelled order handling |
| Insufficient balance | Order exceeding balance | Error response parsing |
| Invalid ticker | Malformed ticker | Error handling |
| Market closed | Order on settled market | Error handling |

**Polymarket (second priority):**

| Scenario | How to trigger | What it tests |
|----------|----------------|---------------|
| Full fill | Small market order | Happy path, EIP-712 signing |
| Partial fill | Larger limit order | Partial handling |
| No fill | Order at stale price | Cancelled order handling |
| Insufficient balance | Order exceeding balance | Error parsing |

**Combined (replay composition):**

| Scenario | Composition | What it tests |
|----------|-------------|---------------|
| Both sides fill | Kalshi full + Poly full | Happy path arb |
| Kalshi fills, Poly fails | Kalshi full + Poly error | Unmatched exposure handling |
| Both partial | Kalshi partial + Poly partial | Imbalanced position |
| Kalshi fails, Poly fills | Kalshi error + Poly full | Reverse exposure |

**Note:** Slippage cannot occur with limit orders (IOC/FAK). Orders fill at the limit price or better, or they don't fill. Only partial/no fills need testing.

## Implementation Phases

### Phase 1: Capture & Targeted Testing

| Step | What | Deliverable |
|------|------|-------------|
| 1.1 | Build `CaptureMiddleware` | `trading/src/capture.rs` |
| 1.2 | Make HTTP client base URLs configurable | Refactor `kalshi.rs`, `polymarket_clob.rs` |
| 1.3 | Wire capture into CLI and controller | `CAPTURE_DIR` env var support |
| 1.4 | Capture real scenarios on Kalshi | Fixture files |
| 1.5 | Build replay test harness with `wiremock` | Integration tests |
| 1.6 | Repeat capture for Polymarket | More fixtures |
| 1.7 | Build combined scenario tests | Cross-platform test coverage |

### Phase 2: Full Pipeline Testing (Follow-on)

| Step | What | Deliverable |
|------|------|-------------|
| 2.1 | Build mock WebSocket server infrastructure | Test utility for fake Kalshi/Poly WS |
| 2.2 | Make WS endpoints configurable | Refactor WS clients |
| 2.3 | Build "arb injection" test harness | Push price updates, observe execution |
| 2.4 | End-to-end scenarios | Full pipeline tests |

Phase 2 tests:
- Arb detection logic under realistic conditions
- Race conditions between detection and execution
- Full integration between all components
- Restart/recovery with open positions

## File Structure

**New files:**

```
trading/
└── src/
    ├── lib.rs              # Add capture module
    └── capture.rs          # CaptureMiddleware + filter logic

controller/
└── tests/
    └── integration/
        ├── mod.rs
        ├── fixtures/
        │   ├── kalshi_full_fill.json
        │   ├── kalshi_partial_fill.json
        │   ├── kalshi_insufficient_balance.json
        │   ├── poly_full_fill.json
        │   ├── poly_partial_fill.json
        │   └── manifest.json
        ├── replay_harness.rs
        ├── test_kalshi_execution.rs
        ├── test_poly_execution.rs
        └── test_combined_scenarios.rs
```

**New dependencies:**

```toml
# trading/Cargo.toml
[dependencies]
reqwest-middleware = "0.2"

# controller/Cargo.toml
[dev-dependencies]
wiremock = "0.5"
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CAPTURE_DIR` | unset | Enables capture, sets output directory |
| `CAPTURE_FILTER` | `orders` | What to capture: `orders`, `all`, or custom regex |

## Testing Budget

$50-200 allocated for learning losses during manual trade capture phase.

## Platform Priority

1. Kalshi (US-regulated, clearer error messages)
2. Polymarket (after Kalshi patterns established)
