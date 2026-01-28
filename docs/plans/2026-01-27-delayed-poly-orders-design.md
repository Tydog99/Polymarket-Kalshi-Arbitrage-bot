# Design: Handle Delayed Polymarket Orders

## Problem

When Polymarket returns `status=delayed`, the bot treats it as 0 fills and triggers premature auto-close on Kalshi. But delayed orders can eventually fill, leaving us with sold Kalshi shares we actually needed.

## Solution

Spawn a background reconciliation task for delayed Poly orders instead of blocking main execution or triggering premature auto-close.

```
Current Flow:
execute_both_legs_async() → [Kalshi=10, Poly=0 delayed] → mismatch! → auto-close Kalshi ❌

New Flow:
execute_both_legs_async() → [Kalshi=10, Poly=delayed]
    → record Kalshi with pending marker
    → spawn reconciliation task (non-blocking)
    → task polls Poly → resolved → record Poly fill, clear pending
    → if mismatch → unwind Kalshi excess
```

## Design Decisions

| Decision | Choice |
|----------|--------|
| Max poll timeout | 5 seconds (configurable via `POLY_DELAYED_TIMEOUT_MS`) |
| On timeout | Assume no fill, unwind all Kalshi |
| Partial fills | Unwind mismatch only, keep hedged portion |
| Kalshi delays | Not handled (Kalshi IOC is synchronous) |
| Fill recording | Record Kalshi immediately with pending marker |
| Pending marker | `reconciliation_pending: Option<String>` in FillRecord (stores order ID) |
| On restart with pending | Log ERROR with banner, require manual action |

## Data Structures

**PolyFillAsync** (polymarket_clob.rs):
```rust
pub struct PolyFillAsync {
    pub order_id: String,
    pub filled_size: f64,
    pub fill_cost: f64,
    pub is_delayed: bool,  // NEW
}
```

**FillRecord** (position_tracker.rs):
```rust
pub struct FillRecord {
    // ... existing fields ...
    pub reconciliation_pending: Option<String>,  // NEW: Some(poly_order_id) if pending
}
```

## Polling Logic

```rust
pub async fn poll_delayed_order(
    &self,
    order_id: &str,
    price: f64,
    timeout_ms: u64,
) -> Result<(f64, f64)> {
    let start = Instant::now();
    let delays = [100, 200, 500, 1000, 1500, 2000]; // ms backoff
    let mut attempt = 0;

    while start.elapsed().as_millis() < timeout_ms as u128 {
        let delay_ms = delays.get(attempt).copied().unwrap_or(2000);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        attempt += 1;

        match self.inner.get_order_async(order_id, &self.creds).await {
            Ok(resp) => match resp.status.as_str() {
                "matched" | "filled" => {
                    let filled: f64 = resp.size_matched.parse().unwrap_or(0.0);
                    return Ok((filled, filled * price));
                }
                "canceled" | "expired" => return Ok((0.0, 0.0)),
                _ => continue,
            },
            Err(e) => {
                warn!("[POLY] Poll attempt {} failed: {}", attempt, e);
                continue;
            }
        }
    }

    Err(anyhow!("Order {} still pending after {}ms", order_id, timeout_ms))
}
```

## Execution Flow

In `process()` after `execute_both_legs_async()`:

```rust
let (..., poly_delayed) = result;

if poly_delayed {
    info!("[EXEC] ⏳ Poly order delayed ({}), spawning reconciliation", poly_order_id);

    // Record Kalshi fill immediately WITH pending marker
    position_channel.record_fill(FillRecord {
        // ...kalshi fill details...
        reconciliation_pending: Some(poly_order_id.clone()),
    });

    // Spawn background reconciliation - doesn't block
    tokio::spawn(reconcile_delayed_poly(...));

    return Ok(ExecutionResult { market_id, success: true, ... });
}

// Existing mismatch/auto-close logic for non-delayed orders
```

## Reconciliation Function

```rust
async fn reconcile_delayed_poly(
    poly_async: Arc<PolyAsync>,
    kalshi: Arc<KalshiClient>,
    poly_order_id: String,
    poly_price: i64,
    kalshi_filled: i64,
    pair: MarketPair,
    arb_type: ArbType,
    position_channel: PositionChannel,
    timeout_ms: u64,
) {
    let price_decimal = poly_price as f64 / 100.0;

    info!("[RECONCILE] Polling Poly order {} (Kalshi filled {})",
          poly_order_id, kalshi_filled);

    let poly_filled = match poly_async.poll_delayed_order(&poly_order_id, price_decimal, timeout_ms).await {
        Ok((filled, _cost)) => filled as i64,
        Err(e) => {
            error!("[RECONCILE] ❌ Timeout polling {}: {} - assuming no fill", poly_order_id, e);
            0
        }
    };

    // Record Poly fill
    position_channel.record_fill(FillRecord {
        // ...poly fill details...
        reconciliation_pending: None,
    });

    // Clear pending marker on Kalshi fill
    position_channel.clear_reconciliation_pending(&poly_order_id);

    info!("[RECONCILE] ✅ Poly {} resolved: filled={}", poly_order_id, poly_filled);

    // Handle mismatch using existing auto-close logic
    if kalshi_filled != poly_filled {
        let excess = kalshi_filled - poly_filled;
        warn!("[RECONCILE] ⚠️ Mismatch: Kalshi={} Poly={}, unwinding {} Kalshi contracts",
              kalshi_filled, poly_filled, excess);

        close_kalshi_with_retry(..., excess, ...).await;
    }
}
```

## Startup Warning

```rust
impl PositionTracker {
    pub fn check_pending_reconciliations(&self) {
        let pending: Vec<_> = self.fills.iter()
            .filter(|f| f.reconciliation_pending.is_some())
            .collect();

        if !pending.is_empty() {
            error!("╔══════════════════════════════════════════════════════════════╗");
            error!("║  ⚠️  PENDING RECONCILIATIONS DETECTED FROM PREVIOUS SESSION  ║");
            error!("╠══════════════════════════════════════════════════════════════╣");
            for fill in &pending {
                error!("║  Poly Order: {}  ", fill.reconciliation_pending.as_ref().unwrap());
                error!("║  Market: {} | Kalshi filled: {}  ", fill.market_id, fill.filled_contracts);
            }
            error!("╠══════════════════════════════════════════════════════════════╣");
            error!("║  ACTION REQUIRED: Manually verify Polymarket order status    ║");
            error!("║  and reconcile positions before continuing trading.          ║");
            error!("╚══════════════════════════════════════════════════════════════╝");
        }
    }
}
```

## Files to Modify

| File | Changes |
|------|---------|
| `controller/src/polymarket_clob.rs` | Add `is_delayed` to `PolyFillAsync`, add `poll_delayed_order()`, suppress warnings for delayed |
| `controller/src/execution.rs` | Propagate delayed flag, spawn reconciliation task, add `reconcile_delayed_poly()` |
| `controller/src/position_tracker.rs` | Add `reconciliation_pending` to `FillRecord`, add `clear_reconciliation_pending()`, add `check_pending_reconciliations()` |
| `controller/src/main.rs` | Call `check_pending_reconciliations()` on startup, read `POLY_DELAYED_TIMEOUT_MS` env var |

## Configuration

| Env Var | Default | Description |
|---------|---------|-------------|
| `POLY_DELAYED_TIMEOUT_MS` | 5000 | Max time to poll a delayed Poly order before assuming no fill |

## Verification

1. **Unit test**: Mock delayed Poly response, verify reconciliation spawned
2. **Integration test**: Verify polling, position recording, and mismatch handling
3. **Live test (dry run)**: Look for `[RECONCILE]` logs, verify no premature auto-close
