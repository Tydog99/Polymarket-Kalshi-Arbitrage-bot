# TODO (Running)

Last updated: 2026-01-20

## Active work

- [ ] **Capture Polymarket buy and sell events**
  - [ ] Persist fills/trades (buy + sell) in capture/replay format
  - [ ] Include order id, market/token, side (buy/sell), price, size, timestamp, status transitions

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

- [] **Figure out the right processes to rebalance accounts within the legal bounds**

- [] **Push Notifications to our phones for arbs**

## Notes / decisions

- Keep this file as the source of truth for “what’s next” and mark items done as we land changes.

