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

- [ ] **Positions.json improvements
 - [ ] Positions are never closed
 - [ ] Reconcile positions on Kalshi (via Kalshi order info? portfolio API req?)
 - [ ] Reconcile positions on Poly (via Poly order id? portfolio API req?) (This might require a human to 'redeem' YES/NO tokens after an event closes first.)

- [ ] **Circuit breaker improvement
  - [ ] Each app start has no understanding of current open positions. It should read positions.json

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

## Notes / decisions

- Keep this file as the source of truth for “what’s next” and mark items done as we land changes.

