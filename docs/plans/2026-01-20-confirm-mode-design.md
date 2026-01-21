# Controller Confirm Mode Design

**Date:** 2026-01-20
**Status:** Approved
**Goal:** Add a mode requiring explicit confirmation before dispatching execution legs, allowing gradual trust-building in the system.

## Overview

Confirm mode introduces a confirmation layer between arb detection and execution. When enabled for a league, detected arbitrage opportunities are queued for manual approval rather than auto-executing. A split-pane TUI displays pending opportunities with live price updates.

## Configuration

### League-level setting

Each league in `config.rs` gets a `confirm_mode: bool` field, defaulting to `true`:

```rust
LeagueConfig {
    name: "nba",
    confirm_mode: true,  // default true for all leagues
    // ... other fields
}
```

### Runtime overrides

- CLI: `--confirm-mode-skip=nba,nfl`
- Env: `CONFIRM_MODE_SKIP=nba,nfl`

Leagues in the skip list bypass confirmation and auto-execute. CLI takes precedence over env var.

### Effective logic

```rust
fn requires_confirmation(league: &str) -> bool {
    let league_config = get_league_config(league);
    if !league_config.confirm_mode {
        return false;
    }
    !is_in_skip_list(league)
}
```

When all leagues are skipped, the TUI never launches and the bot runs exactly as today.

## TUI Design

### Layout

Split-pane terminal UI using `ratatui`:

```
┌─────────────────────────────────────────────────────────────────────┐
│ [INFO] WebSocket connected to Kalshi                                │
│ [INFO] Discovered 47 market pairs                                   │
│ [EXEC] 📡 Detected: DEN vs WAS | poly_yes_kalshi_no y=52¢ n=47¢    │
│ [EXEC] ⏸️  Queued for confirmation (nba)                            │
│ ...logs continue scrolling...                                       │
├─────────────────────────────────────────────────────────────────────┤
│ 🔔 PENDING (1 of 2)                                                 │
│                                                                     │
│ NBA - Denver Nuggets vs Washington Wizards (Spread -12.5)           │
│ BUY Poly YES (51¢) + BUY Kalshi NO (47¢) = 98¢ → 2¢ profit          │
│ Size: 10 contracts | Est. profit: 20¢                               │
│ 🔄 Detected 6x in last 45s                                          │
│                                                                     │
│ Kalshi:  https://kalshi.com/markets/KXNBA...                        │
│ Poly:    https://polymarket.com/event/nba-den...                    │
│                                                                     │
│  > [Proceed]   [Reject]   [Reject + Blacklist]                      │
│    ←/→ select │ Enter confirm │ n = add note │ q = quit             │
└─────────────────────────────────────────────────────────────────────┘
```

### Keyboard controls

| Key | Action |
|-----|--------|
| ←/→ | Move selection between options |
| Enter | Confirm selected action |
| n | Add optional note before confirming |
| q | Quit (discards pending confirmations) |

### TUI lifecycle

1. Bot starts with normal scrolling logs
2. First confirm-mode arb detected → TUI launches, takes over terminal
3. User processes all pending arbs → TUI closes, back to normal logs
4. Another confirm-mode arb detected → TUI relaunches
5. Repeat

The TUI uses crossterm's `enable_raw_mode`/`disable_raw_mode` for clean enter/exit transitions.

### Live updates

- Prices in the confirmation panel update in real-time as new orderbook data arrives
- Detection counter increments when same arb is re-detected
- If prices move and arb becomes invalid (yes + no >= 100¢), panel shows "Opportunity expired" and auto-advances to next

## Confirmation Queue

### Data flow

```
Price update → Arb detected → ConfirmationQueue → (user decision) → ExecutionEngine
                                    ↓
                              (if confirm_mode
                               disabled for league)
                                    ↓
                            ExecutionEngine (direct)
```

### Queue behavior

- Keyed by `market_id` - one slot per market maximum
- New arb for existing market replaces old data (keeps prices fresh)
- Increments detection counter on replacement
- Removes entry when: user decides, or arb becomes invalid (prices move)
- No timeout - pending arbs stay until decided or prices invalidate them

### Actions and outcomes

| Action | Condition | Behavior | Status logged |
|--------|-----------|----------|---------------|
| Proceed | Arb still valid | Execute via ExecutionEngine | `accepted` |
| Proceed | Prices moved, arb gone | Skip execution, advance | `accepted_expired` |
| Reject | - | Remove from queue, advance | `rejected` |
| Reject + Blacklist | - | Remove, suppress market 5min | `blacklisted` |
| (Auto) | Prices invalidated while pending | Remove, advance | `expired` |

### Re-validation on proceed

1. Re-fetch current prices from `GlobalState` orderbook cache
2. Validate arb still exists (yes + no < 100¢)
3. If valid → send to `ExecutionEngine::process()`
4. If invalid → log as `accepted_expired`, advance to next

### Blacklist

- 5-minute duration only (no configurable levels)
- In-memory only (resets on restart)
- Prevents same market from re-entering queue during blacklist period

## Logging

### File format

One file per session: `confirmations_2026-01-20_14-30-00.json`
Stored alongside `positions.json`

### Record structure

```json
{
  "timestamp": "2026-01-20T14:32:15.123Z",
  "status": "rejected",
  "market_id": 42,
  "pair_id": "nba-den-was-2026-01-20-spread-home-12pt5",
  "description": "NBA - Denver Nuggets vs Washington Wizards",
  "arb_type": "poly_yes_kalshi_no",
  "yes_price_cents": 52,
  "no_price_cents": 47,
  "profit_cents": 1,
  "max_contracts": 10,
  "detection_count": 4,
  "kalshi_url": "https://kalshi.com/markets/KXNBA...",
  "poly_url": "https://polymarket.com/event/nba-den...",
  "note": "Line looks wrong, waiting for correction"
}
```

### Status values

- `accepted` - User proceeded, arb was valid, sent to execution
- `accepted_expired` - User proceeded, but prices had moved, skipped
- `rejected` - User rejected (can re-queue on next detection)
- `blacklisted` - User rejected with 5-min blacklist
- `expired` - Prices invalidated while pending (auto-removed)

## Module Structure

### New files

| File | Purpose |
|------|---------|
| `confirm_queue.rs` | `ConfirmationQueue` struct, pending arb storage, blacklist tracking |
| `confirm_tui.rs` | ratatui-based split-pane UI, keyboard handling, log buffer |
| `confirm_log.rs` | `confirmations.json` read/write, record types |

### Changes to existing files

| File | Change |
|------|--------|
| `config.rs` | Add `confirm_mode: bool` to `LeagueConfig`, add `CONFIRM_MODE_SKIP` parsing |
| `main.rs` | Conditionally launch TUI event loop, wire up `ConfirmationQueue` between detection and execution |
| `Cargo.toml` | Add `ratatui`, `crossterm` dependencies |

### Dependencies

```toml
ratatui = "0.29"
crossterm = "0.28"
```

## Remote Trader Integration

Confirmation happens on the controller side only. The remote trader remains a pure executor - it receives execute commands after the controller has confirmed.

Flow:
1. Controller detects arb
2. Controller queues for confirmation (if confirm_mode enabled)
3. User approves on controller
4. Controller sends execute command to remote trader
5. Remote trader executes without its own confirmation

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| User quits (q) with pending arbs | Pending arbs discarded (not logged). Clean shutdown. |
| Controller crashes mid-session | Pending arbs lost. Blacklist resets. |
| Execution fails after "Proceed" | Normal execution error handling (circuit breaker, auto-close). Confirmation was successful. |
| All leagues in skip list | TUI never launches, bot runs as today. |
| Prices update while typing note | Note input is modal. Display refreshes after note submitted/cancelled. |
| Remote trader disconnected | Confirmation works. Execution fails with connection error. |
