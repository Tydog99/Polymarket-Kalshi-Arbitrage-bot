# Confirmation TUI & Logging Architecture

## Overview

The controller uses a split-pane TUI (ratatui) for manual arbitrage confirmation:
- **Upper pane (65%)**: Scrolling log buffer
- **Lower pane (35%)**: Confirmation dialog with arb details and action menu

## Key Files

| File | Purpose |
|------|---------|
| `confirm_tui.rs` | TUI rendering, input handling, log routing |
| `confirm_queue.rs` | Queue of pending arbs awaiting confirmation |
| `confirm_log.rs` | Persistence of confirmation decisions to JSON |
| `main.rs:init_logging()` | Tracing subscriber setup with TUI-aware writer |

## Log Routing Architecture

**Problem solved**: When TUI is active, normal `info!()` calls would write to stdout and corrupt the ratatui display.

**Solution**: `TuiAwareWriter` dynamically routes tracing output based on TUI state.

```
┌─────────────────────────────────────────────────────────────┐
│                    tracing subscriber                        │
├─────────────────────────────────────────────────────────────┤
│  console_layer ──► TuiAwareWriter                           │
│                      │                                       │
│                      ├─ TUI inactive → stdout                │
│                      └─ TUI active   → TUI_LOG_TX channel    │
│                                              │               │
│  file_layer ────► .logs/controller-*.log     │               │
└─────────────────────────────────────────────│───────────────┘
                                               ▼
                                    ┌──────────────────┐
                                    │  TUI log_rx      │
                                    │  (confirm_tui)   │
                                    └────────┬─────────┘
                                             ▼
                                    ┌──────────────────┐
                                    │  Upper log pane  │
                                    └──────────────────┘
```

## Global State (confirm_tui.rs)

```rust
// Flag checked by TuiAwareWriter on every write
pub static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

// Channel sender for routing logs to TUI
static TUI_LOG_TX: OnceLock<mpsc::Sender<String>> = OnceLock::new();
```

## Initialization Flow (main.rs)

```rust
// 1. Create log channel
let (tui_log_tx, tui_log_rx) = mpsc::channel::<String>(1024);

// 2. Register with global state so TuiAwareWriter can access it
confirm_tui::init_tui_log_channel(tui_log_tx.clone());

// 3. Console layer uses TuiAwareWriter instead of default stdout
let console_layer = tracing_subscriber::fmt::layer()
    .with_writer(confirm_tui::TuiAwareWriter);
```

## TUI Lifecycle

```rust
// In run_tui():
// On start:
TUI_ACTIVE.store(true, Ordering::Relaxed);

// On exit:
TUI_ACTIVE.store(false, Ordering::Relaxed);
```

## Why This Design?

1. **No changes needed to logging code** - All existing `info!()`, `warn!()`, `error!()` calls work automatically
2. **Logs always go to file** - File layer is unaffected
3. **Zero log loss** - When TUI is active, logs route to channel instead of being dropped
4. **Minimal overhead** - Single atomic load per log line

## Confirmation Queue Flow

```
Arb detected (heartbeat loop)
        │
        ▼
ConfirmationQueue::add()
        │
        ▼
TUI displays in lower pane
        │
        ▼
User: [Proceed] / [Reject] / [Reject + Blacklist]
        │
        ▼
ConfirmAction sent via channel
        │
        ▼
main.rs processes action:
  - Proceed → forward to execution
  - Reject → log and discard
  - Blacklist → add to blacklist + discard
```

## Configuration

| Env Var | Default | Description |
|---------|---------|-------------|
| `CONFIRM_MODE` | `1` | Enable confirmation TUI |
| `CONFIRM_DETECT_COUNT` | `2` | Detections before auto-approve |
| `CONFIRM_TIMEOUT_SECS` | `30` | Timeout before auto-reject |

## Debugging

If logs appear corrupted in TUI:
1. Check `TUI_ACTIVE` is being set correctly in `run_tui()`
2. Verify `init_tui_log_channel()` is called before any logging
3. Ensure console layer uses `TuiAwareWriter`

If logs don't appear in TUI pane:
1. Check channel capacity (1024) isn't exceeded
2. Verify `log_rx` is being drained in TUI event loop
