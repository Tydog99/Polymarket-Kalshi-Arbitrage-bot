# Discovery Latency Analysis

This document analyzes the market discovery latency and documents implemented optimizations.

## Performance Results

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| Total discovery time | ~2 minutes | ~26 seconds | **4.6x faster** |
| Kalshi API calls per series | N+1 (26 for 25 events) | 1 | **26x fewer** |
| Gamma API calls per batch | N (one per slug) | 1-2 (batched) | **15-50x fewer** |
| Markets discovered | 567 pairs | 567 pairs | Same coverage |

**Target achieved:** Reduced discovery from ~2 minutes to <30 seconds.

## Implemented Optimizations

### 1. Batch Gamma Slug Lookups (DONE)

**PR:** #26

Added `lookup_markets_batch()` to batch up to 50 slugs per request:

```rust
// Before: N HTTP requests
for slug in slugs {
    gamma.lookup_market(&slug).await;  // 1 request each
}

// After: 1-2 HTTP requests
gamma.lookup_markets_batch(&slugs).await;  // Batched
```

**Impact:** 100 slugs = 2 requests instead of 100. Eliminates Cloudflare throttling.

### 2. Remove Gamma Semaphore (DONE)

**PR:** #26

With batch lookups, per-request concurrency limiting is unnecessary:

```rust
// Before
gamma_semaphore: Arc<Semaphore::new(GAMMA_CONCURRENCY)>,  // 5 concurrent

// After
// Note: gamma_semaphore removed - batch lookups don't need per-request limiting
```

### 3. Direct Series Market Fetch (DONE)

**PR:** #26

Use `get_markets_for_series()` for all series, not just MVE:

```rust
// Before: N+1 calls
let events = kalshi.get_events(series).await;           // 1 call
for event in events {
    kalshi.get_markets(&event.event_ticker).await;      // N calls
}

// After: 1 call
let markets = kalshi.get_markets_for_series(series).await;  // 1 call
// Group by event_ticker in memory
```

**Impact:** 25 events = 1 request instead of 26.

### 4. Esports Fast Path (DONE)

**PR:** #26

Converted esports discovery from N+1 to 2 calls:

```rust
// Before: N+1 calls
let events = kalshi.get_events(series).await;       // 1 call
for event in events {
    if poly_lookup.contains(event) {
        kalshi.get_markets(&event.ticker).await;    // N calls
    }
}

// After: 2 calls
let events = kalshi.get_events(series).await;       // 1 call (for titles)
let markets = kalshi.get_markets_for_series(series).await;  // 1 call
// Group markets by event_ticker, match in memory
```

## Current Architecture

```
For each league (parallel via join_all):
  For each market_type (parallel via join_all):
    1. Fetch ALL markets: GET /markets?series_ticker=X (1 request)
    2. Group by event_ticker in memory
    3. Build Polymarket slugs
    4. Batch Gamma lookup (1-2 requests per 50 slugs)
```

## Remaining Optimizations

### TODO: Parallel Market Type Discovery (Low Priority)

Market types are already parallelized within each league. Further optimization would require careful rate limit handling and diminishing returns given current performance.

## API Rate Limits Reference

### Kalshi

| Setting | Value | Location |
|---------|-------|----------|
| `KALSHI_RATE_LIMIT_PER_SEC` | 2 | `discovery.rs:31` |
| `KALSHI_GLOBAL_CONCURRENCY` | 1 | `discovery.rs:35` |

### Polymarket Gamma API

From [Polymarket docs](https://docs.polymarket.com/quickstart/introduction/rate-limits):

| Endpoint | Limit |
|----------|-------|
| `/markets` | 300 requests / 10s (30 req/sec) |
| `/events` | 500 requests / 10s (50 req/sec) |
| General | 4000 requests / 10s (400 req/sec) |

**Note:** The real constraint is Cloudflare's anti-bot protection, not Polymarket's rate limits. Batching reduces request count, which avoids triggering Cloudflare.
