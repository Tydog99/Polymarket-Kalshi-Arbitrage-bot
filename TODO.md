# TODO (Running)

Last updated: 2026-01-20

## Active work

- [x] **Capture Polymarket buy and sell events**
  - [x] Persist fills/trades (buy + sell) in capture/replay format
  - [x] Include order id, market/token, side (buy/sell), price, size, timestamp, status transitions

- [ ] **Web app: positions + drift detection**
  - [ ] Pull live positions/balances from **Polymarket + Kalshi APIs**
  - [ ] Load local `positions.json`
  - [ ] Compute and display **drift** (per market + totals) and flag mismatches
  - [ ] Provide “reconcile hints” (which leg/side is missing)

- [x] **Controller confirm mode**
  - [x] Add a mode that requires explicit confirmation before dispatching execution legs
  - [x] Ensure it works for cross-platform + same-platform arbs (and remote-trader routing)

- [x] **Integration test: exposure sell failure is handled gracefully** (PR #18)
  - [x] Simulate one leg filling and the compensating sell failing
  - [x] Verify we record exposure, trip appropriate safeguards, and keep system running

- [ ] **Check cash balances on startup**
  - [ ] Query Kalshi and Polymarket cash balances on controller start
  - [ ] Use as upper bound for money to be spent

- [ ] **Continuously monitor cash balances**
  - [ ] Pull cash balance periodically over app lifecycle
  - [ ] Update spending limits dynamically

- [ ] **Figure out the right processes to rebalance accounts within the legal bounds**

- [ ] **Push Notifications to our phones for arbs**

- [ ] **Monitor open positions for premature leg closure**
  - [ ] Scan positions periodically for one leg closing early (e.g., market resolves on one platform)
  - [ ] Surface this information to operator (logs, alerts, TUI)
  - [ ] Optionally take action (close remaining leg, flag for manual review)

- [ ] **Audit circuit breaker behavior for repeated Poly failures**
  - [ ] Understand current flow: arb detected → Kalshi fills → Poly fails → Kalshi unwinds at loss → arb re-appears → repeat
  - [ ] Determine if market-specific blacklisting exists (or should exist)
  - [ ] Review cooldown logic: does consecutive error tracking prevent this loop?
  - [ ] Document circuit_breaker.rs behavior for this scenario

- [ ] **Document Polymarket order placement flow (3 API calls)**
  - [ ] Explain pre-signing API calls and why they're needed
  - [ ] Document the sequence: create order → sign → submit (or similar)

## Notes / decisions

- Keep this file as the source of truth for “what’s next” and mark items done as we land changes.

