# Confirm Mode Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a confirmation mode that requires explicit user approval before executing arbitrage trades, with a split-pane TUI for real-time decision making.

**Architecture:** A `ConfirmationQueue` sits between arb detection and `ExecutionEngine`. When confirm mode is enabled for a league, arbs queue for approval instead of auto-executing. A ratatui-based TUI launches when the first arb needs confirmation, closes when the queue empties.

**Tech Stack:** Rust, ratatui (TUI framework), crossterm (terminal backend), tokio (async runtime)

---

## Task 1: Add Dependencies

**Files:**
- Modify: `controller/Cargo.toml`

**Step 1: Add ratatui and crossterm dependencies**

Add to `[dependencies]` section in `controller/Cargo.toml`:

```toml
ratatui = "0.29"
crossterm = "0.28"
```

**Step 2: Verify dependencies resolve**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 3: Commit**

```bash
git add controller/Cargo.toml
git commit -m "deps: add ratatui and crossterm for confirm mode TUI"
```

---

## Task 2: Add confirm_mode to LeagueConfig

**Files:**
- Modify: `controller/src/config.rs:170-187` (LeagueConfig struct)
- Modify: `controller/src/config.rs:190-400` (all league definitions)

**Step 1: Add confirm_mode field to LeagueConfig**

In `controller/src/config.rs`, update the `LeagueConfig` struct:

```rust
/// League configuration for market discovery
#[derive(Debug, Clone)]
pub struct LeagueConfig {
    pub league_code: &'static str,
    pub poly_prefix: &'static str,
    pub kalshi_series_game: &'static str,
    pub kalshi_series_spread: Option<&'static str>,
    pub kalshi_series_total: Option<&'static str>,
    pub kalshi_series_btts: Option<&'static str>,
    pub poly_series_id: Option<&'static str>,
    pub kalshi_web_slug: &'static str,
    pub home_team_first: bool,
    /// Require manual confirmation before executing trades for this league
    pub confirm_mode: bool,
}
```

**Step 2: Add confirm_mode: true to all league definitions**

Update every `LeagueConfig { ... }` in `get_league_configs()` to include `confirm_mode: true,`. For example:

```rust
LeagueConfig {
    league_code: "epl",
    poly_prefix: "epl",
    kalshi_series_game: "KXEPLGAME",
    kalshi_series_spread: Some("KXEPLSPREAD"),
    kalshi_series_total: Some("KXEPLTOTAL"),
    kalshi_series_btts: Some("KXEPLBTTS"),
    poly_series_id: None,
    kalshi_web_slug: "premier-league-game",
    home_team_first: true,
    confirm_mode: true,
},
```

**Step 3: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 4: Commit**

```bash
git add controller/src/config.rs
git commit -m "feat(config): add confirm_mode field to LeagueConfig (default true)"
```

---

## Task 3: Add CONFIRM_MODE_SKIP parsing

**Files:**
- Modify: `controller/src/config.rs` (add new function after `disabled_leagues()`)

**Step 1: Add confirm_mode_skip_leagues function**

Add after the `is_league_disabled()` function (~line 136):

```rust
use std::collections::HashSet;

/// Leagues that skip confirmation mode (auto-execute)
/// Set CONFIRM_MODE_SKIP env var to comma-separated list, e.g., "nba,nfl"
pub fn confirm_mode_skip_leagues() -> &'static HashSet<String> {
    static CACHED: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    CACHED.get_or_init(|| {
        std::env::var("CONFIRM_MODE_SKIP")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|l| l.trim().to_lowercase()).collect())
            .unwrap_or_default()
    })
}

/// Check if a league requires confirmation before trading
/// Returns true if league has confirm_mode=true AND is not in CONFIRM_MODE_SKIP list
pub fn requires_confirmation(league: &str) -> bool {
    let league_lower = league.to_lowercase();

    // Check if explicitly skipped
    if confirm_mode_skip_leagues().contains(&league_lower) {
        return false;
    }

    // Check league config
    get_league_configs()
        .iter()
        .find(|c| c.league_code == league_lower)
        .map(|c| c.confirm_mode)
        .unwrap_or(true) // Default to requiring confirmation for unknown leagues
}

/// Check if any league requires confirmation
pub fn any_league_requires_confirmation() -> bool {
    get_league_configs()
        .iter()
        .any(|c| c.confirm_mode && !confirm_mode_skip_leagues().contains(&c.league_code.to_lowercase()))
}
```

**Step 2: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 3: Commit**

```bash
git add controller/src/config.rs
git commit -m "feat(config): add CONFIRM_MODE_SKIP env var parsing"
```

---

## Task 4: Create confirmation log types (confirm_log.rs)

**Files:**
- Create: `controller/src/confirm_log.rs`

**Step 1: Create the confirmation log module**

Create `controller/src/confirm_log.rs`:

```rust
//! Confirmation logging for audit trail of user decisions.
//!
//! Records all confirmation decisions (accept/reject/blacklist) to a timestamped
//! JSON file for later analysis.

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use crate::types::ArbType;

/// Status of a confirmation decision
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationStatus {
    /// User approved and arb was still valid
    Accepted,
    /// User approved but prices had moved (arb expired)
    AcceptedExpired,
    /// User rejected (can re-queue on next detection)
    Rejected,
    /// User rejected with 5-minute blacklist
    Blacklisted,
    /// Prices invalidated while pending (auto-removed)
    Expired,
}

/// A single confirmation record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationRecord {
    pub timestamp: DateTime<Utc>,
    pub status: ConfirmationStatus,
    pub market_id: u16,
    pub pair_id: String,
    pub description: String,
    pub league: String,
    pub arb_type: ArbType,
    pub yes_price_cents: u16,
    pub no_price_cents: u16,
    pub profit_cents: i16,
    pub max_contracts: i64,
    pub detection_count: u32,
    pub kalshi_url: String,
    pub poly_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Logger that appends confirmation records to a session file
pub struct ConfirmationLogger {
    writer: BufWriter<File>,
    file_path: PathBuf,
    record_count: usize,
}

impl ConfirmationLogger {
    /// Create a new logger with a timestamped filename
    pub fn new() -> Result<Self> {
        let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
        let filename = format!("confirmations_{}.json", timestamp);
        let file_path = PathBuf::from(&filename);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;

        let writer = BufWriter::new(file);

        Ok(Self {
            writer,
            file_path,
            record_count: 0,
        })
    }

    /// Log a confirmation record
    pub fn log(&mut self, record: ConfirmationRecord) -> Result<()> {
        let json = serde_json::to_string(&record)?;
        writeln!(self.writer, "{}", json)?;
        self.writer.flush()?;
        self.record_count += 1;
        Ok(())
    }

    /// Get the path to the log file
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }

    /// Get the number of records logged
    pub fn record_count(&self) -> usize {
        self.record_count
    }
}
```

**Step 2: Add Serialize/Deserialize to ArbType in types.rs**

In `controller/src/types.rs`, update the `ArbType` enum to derive Serialize and Deserialize:

```rust
/// Arbitrage opportunity type, determining the execution strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArbType {
    /// Cross-platform: Buy Polymarket YES + Buy Kalshi NO
    PolyYesKalshiNo,
    /// Cross-platform: Buy Kalshi YES + Buy Polymarket NO
    KalshiYesPolyNo,
    /// Same-platform: Buy Polymarket YES + Buy Polymarket NO
    PolyOnly,
    /// Same-platform: Buy Kalshi YES + Buy Kalshi NO
    KalshiOnly,
}
```

**Step 3: Add module to lib.rs or main.rs**

In `controller/src/main.rs`, add the module declaration after the other `mod` statements:

```rust
mod confirm_log;
```

**Step 4: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 5: Commit**

```bash
git add controller/src/confirm_log.rs controller/src/types.rs controller/src/main.rs
git commit -m "feat(confirm): add confirmation logging types and writer"
```

---

## Task 5: Create PendingArb and ConfirmationQueue types (confirm_queue.rs)

**Files:**
- Create: `controller/src/confirm_queue.rs`

**Step 1: Create the confirmation queue module**

Create `controller/src/confirm_queue.rs`:

```rust
//! Confirmation queue for pending arbitrage opportunities.
//!
//! Manages a keyed queue of arbs awaiting user confirmation. One slot per market,
//! new arbs for existing markets replace old data to keep prices fresh.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use crate::config::{build_polymarket_url, KALSHI_WEB_BASE};
use crate::types::{ArbType, FastExecutionRequest, GlobalState, MarketPair, PriceCents};

/// A pending arbitrage opportunity awaiting confirmation
#[derive(Debug, Clone)]
pub struct PendingArb {
    /// Original execution request
    pub request: FastExecutionRequest,
    /// Market pair info (for display)
    pub pair: Arc<MarketPair>,
    /// Number of times this arb has been detected while pending
    pub detection_count: u32,
    /// First detection timestamp
    pub first_detected: Instant,
    /// Most recent detection timestamp
    pub last_detected: Instant,
    /// Kalshi market URL
    pub kalshi_url: String,
    /// Polymarket event URL
    pub poly_url: String,
}

impl PendingArb {
    pub fn new(request: FastExecutionRequest, pair: Arc<MarketPair>) -> Self {
        let kalshi_url = format!("{}/{}", KALSHI_WEB_BASE, pair.kalshi_market_ticker);
        let poly_url = build_polymarket_url(&pair.league, &pair.poly_slug);
        let now = Instant::now();

        Self {
            request,
            pair,
            detection_count: 1,
            first_detected: now,
            last_detected: now,
            kalshi_url,
            poly_url,
        }
    }

    /// Update with fresher price data
    pub fn update(&mut self, request: FastExecutionRequest) {
        self.request = request;
        self.detection_count += 1;
        self.last_detected = Instant::now();
    }

    /// Calculate profit in cents based on current request prices
    pub fn profit_cents(&self) -> i16 {
        self.request.profit_cents()
    }

    /// Calculate max contracts from size
    pub fn max_contracts(&self) -> i64 {
        (self.request.yes_size.min(self.request.no_size) / 100) as i64
    }
}

/// User action on a pending arb
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Approve execution
    Proceed,
    /// Reject (can re-queue on next detection)
    Reject { note: Option<String> },
    /// Reject and blacklist for 5 minutes
    Blacklist { note: Option<String> },
}

/// Blacklist entry for temporarily suppressed markets
struct BlacklistEntry {
    expires: Instant,
}

/// Queue of pending arbitrage opportunities keyed by market_id
pub struct ConfirmationQueue {
    /// Global state for price lookups
    state: Arc<GlobalState>,
    /// Pending arbs keyed by market_id (one slot per market)
    pending: RwLock<HashMap<u16, PendingArb>>,
    /// Order of market_ids for display (first-in order)
    order: RwLock<VecDeque<u16>>,
    /// Blacklisted markets (market_id -> expiry)
    blacklist: RwLock<HashMap<u16, BlacklistEntry>>,
    /// Channel to notify TUI of updates
    update_tx: mpsc::Sender<()>,
}

impl ConfirmationQueue {
    pub fn new(state: Arc<GlobalState>, update_tx: mpsc::Sender<()>) -> Self {
        Self {
            state,
            pending: RwLock::new(HashMap::new()),
            order: RwLock::new(VecDeque::new()),
            blacklist: RwLock::new(HashMap::new()),
            update_tx,
        }
    }

    /// Push a new arb opportunity (or update existing for same market)
    pub async fn push(&self, request: FastExecutionRequest, pair: Arc<MarketPair>) {
        let market_id = request.market_id;

        // Check blacklist
        {
            let blacklist = self.blacklist.read().await;
            if let Some(entry) = blacklist.get(&market_id) {
                if entry.expires > Instant::now() {
                    return; // Still blacklisted
                }
            }
        }

        // Clean expired blacklist entries periodically
        {
            let mut blacklist = self.blacklist.write().await;
            let now = Instant::now();
            blacklist.retain(|_, v| v.expires > now);
        }

        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        if let Some(existing) = pending.get_mut(&market_id) {
            // Update existing entry with fresh prices
            existing.update(request);
        } else {
            // New entry
            pending.insert(market_id, PendingArb::new(request, pair));
            order.push_back(market_id);
        }

        // Notify TUI
        let _ = self.update_tx.try_send(());
    }

    /// Get the current front pending arb (if any)
    pub async fn front(&self) -> Option<PendingArb> {
        let pending = self.pending.read().await;
        let order = self.order.read().await;

        order.front().and_then(|id| pending.get(id).cloned())
    }

    /// Get count of pending arbs
    pub async fn len(&self) -> usize {
        self.pending.read().await.len()
    }

    /// Check if queue is empty
    pub async fn is_empty(&self) -> bool {
        self.pending.read().await.is_empty()
    }

    /// Remove and return the front arb (after user decision)
    pub async fn pop_front(&self) -> Option<PendingArb> {
        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        if let Some(market_id) = order.pop_front() {
            pending.remove(&market_id)
        } else {
            None
        }
    }

    /// Add a market to the blacklist for 5 minutes
    pub async fn blacklist_market(&self, market_id: u16) {
        let mut blacklist = self.blacklist.write().await;
        blacklist.insert(market_id, BlacklistEntry {
            expires: Instant::now() + Duration::from_secs(300), // 5 minutes
        });
    }

    /// Remove a market from pending (e.g., when prices invalidate it)
    pub async fn remove(&self, market_id: u16) -> Option<PendingArb> {
        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        order.retain(|id| *id != market_id);
        pending.remove(&market_id)
    }

    /// Check if arb is still valid (prices haven't moved)
    pub fn validate_arb(&self, arb: &PendingArb) -> bool {
        let market = match self.state.get_by_id(arb.request.market_id) {
            Some(m) => m,
            None => return false,
        };

        // Get current prices from orderbook
        let (k_yes, k_no, _, _) = market.kalshi.load();
        let (p_yes, p_no, _, _) = market.poly.load();

        // Calculate current cost based on arb type
        let current_cost = match arb.request.arb_type {
            ArbType::PolyYesKalshiNo => {
                let fee = crate::types::kalshi_fee_cents(k_no);
                p_yes + k_no + fee
            }
            ArbType::KalshiYesPolyNo => {
                let fee = crate::types::kalshi_fee_cents(k_yes);
                k_yes + fee + p_no
            }
            ArbType::PolyOnly => p_yes + p_no,
            ArbType::KalshiOnly => {
                let fee_yes = crate::types::kalshi_fee_cents(k_yes);
                let fee_no = crate::types::kalshi_fee_cents(k_no);
                k_yes + fee_yes + k_no + fee_no
            }
        };

        // Still profitable if total cost < 100 cents
        current_cost < 100
    }

    /// Get current prices for an arb (for live display updates)
    pub fn get_current_prices(&self, market_id: u16) -> Option<(PriceCents, PriceCents, PriceCents, PriceCents)> {
        let market = self.state.get_by_id(market_id)?;
        let (k_yes, k_no, _, _) = market.kalshi.load();
        let (p_yes, p_no, _, _) = market.poly.load();
        Some((k_yes, k_no, p_yes, p_no))
    }
}
```

**Step 2: Add module to main.rs**

In `controller/src/main.rs`, add after `mod confirm_log;`:

```rust
mod confirm_queue;
```

**Step 3: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 4: Commit**

```bash
git add controller/src/confirm_queue.rs controller/src/main.rs
git commit -m "feat(confirm): add PendingArb and ConfirmationQueue"
```

---

## Task 6: Create TUI module skeleton (confirm_tui.rs)

**Files:**
- Create: `controller/src/confirm_tui.rs`

**Step 1: Create the TUI module with layout**

Create `controller/src/confirm_tui.rs`:

```rust
//! Split-pane TUI for arbitrage confirmation.
//!
//! Upper 2/3: scrolling log buffer
//! Lower 1/3: confirmation panel with pending arb details

use std::io::{self, Stdout};
use std::sync::Arc;
use std::collections::VecDeque;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::sync::{mpsc, RwLock};
use crate::confirm_queue::{ConfirmAction, ConfirmationQueue, PendingArb};

/// Maximum log lines to keep in buffer
const MAX_LOG_LINES: usize = 500;

/// Menu options
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuOption {
    Proceed,
    Reject,
    Blacklist,
}

impl MenuOption {
    fn all() -> &'static [MenuOption] {
        &[MenuOption::Proceed, MenuOption::Reject, MenuOption::Blacklist]
    }

    fn label(&self) -> &'static str {
        match self {
            MenuOption::Proceed => "Proceed",
            MenuOption::Reject => "Reject",
            MenuOption::Blacklist => "Reject + Blacklist",
        }
    }

    fn next(&self) -> MenuOption {
        match self {
            MenuOption::Proceed => MenuOption::Reject,
            MenuOption::Reject => MenuOption::Blacklist,
            MenuOption::Blacklist => MenuOption::Proceed,
        }
    }

    fn prev(&self) -> MenuOption {
        match self {
            MenuOption::Proceed => MenuOption::Blacklist,
            MenuOption::Reject => MenuOption::Proceed,
            MenuOption::Blacklist => MenuOption::Reject,
        }
    }
}

/// TUI state
pub struct TuiState {
    /// Log buffer (ring buffer of recent lines)
    pub log_buffer: VecDeque<String>,
    /// Currently selected menu option
    pub selected: MenuOption,
    /// Note being typed (None if not in note mode)
    pub note_input: Option<String>,
    /// Whether TUI is active
    pub active: bool,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            log_buffer: VecDeque::with_capacity(MAX_LOG_LINES),
            selected: MenuOption::Proceed,
            note_input: None,
            active: false,
        }
    }

    pub fn add_log(&mut self, line: String) {
        if self.log_buffer.len() >= MAX_LOG_LINES {
            self.log_buffer.pop_front();
        }
        self.log_buffer.push_back(line);
    }
}

/// Result of TUI input handling
pub enum TuiResult {
    /// Continue running
    Continue,
    /// User made a decision
    Action(ConfirmAction),
    /// User quit
    Quit,
}

/// Run the TUI event loop
pub async fn run_tui(
    queue: Arc<ConfirmationQueue>,
    state: Arc<RwLock<TuiState>>,
    mut update_rx: mpsc::Receiver<()>,
    action_tx: mpsc::Sender<ConfirmAction>,
) -> anyhow::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    state.write().await.active = true;

    loop {
        // Check if queue is empty - if so, exit TUI
        if queue.is_empty().await {
            break;
        }

        // Get current pending arb
        let pending = queue.front().await;

        // Render
        {
            let tui_state = state.read().await;
            terminal.draw(|f| {
                draw_ui(f, &tui_state, pending.as_ref(), queue.clone());
            })?;
        }

        // Handle input with timeout
        tokio::select! {
            _ = update_rx.recv() => {
                // Queue updated, re-render
                continue;
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                // Check for keyboard input
                if event::poll(std::time::Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        let result = handle_key(key, &state, &pending).await;
                        match result {
                            TuiResult::Continue => {}
                            TuiResult::Action(action) => {
                                action_tx.send(action).await?;
                            }
                            TuiResult::Quit => {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // Cleanup terminal
    state.write().await.active = false;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

async fn handle_key(
    key: KeyEvent,
    state: &Arc<RwLock<TuiState>>,
    pending: &Option<PendingArb>,
) -> TuiResult {
    let mut tui_state = state.write().await;

    // Handle note input mode
    if let Some(ref mut note) = tui_state.note_input {
        match key.code {
            KeyCode::Enter => {
                let note_text = if note.is_empty() { None } else { Some(note.clone()) };
                let action = match tui_state.selected {
                    MenuOption::Proceed => ConfirmAction::Proceed,
                    MenuOption::Reject => ConfirmAction::Reject { note: note_text },
                    MenuOption::Blacklist => ConfirmAction::Blacklist { note: note_text },
                };
                tui_state.note_input = None;
                return TuiResult::Action(action);
            }
            KeyCode::Esc => {
                tui_state.note_input = None;
            }
            KeyCode::Backspace => {
                note.pop();
            }
            KeyCode::Char(c) => {
                note.push(c);
            }
            _ => {}
        }
        return TuiResult::Continue;
    }

    // Normal mode
    match key.code {
        KeyCode::Left => {
            tui_state.selected = tui_state.selected.prev();
        }
        KeyCode::Right => {
            tui_state.selected = tui_state.selected.next();
        }
        KeyCode::Enter => {
            if pending.is_some() {
                let action = match tui_state.selected {
                    MenuOption::Proceed => ConfirmAction::Proceed,
                    MenuOption::Reject => ConfirmAction::Reject { note: None },
                    MenuOption::Blacklist => ConfirmAction::Blacklist { note: None },
                };
                return TuiResult::Action(action);
            }
        }
        KeyCode::Char('n') => {
            // Enter note mode
            tui_state.note_input = Some(String::new());
        }
        KeyCode::Char('q') => {
            return TuiResult::Quit;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return TuiResult::Quit;
        }
        _ => {}
    }

    TuiResult::Continue
}

fn draw_ui(
    f: &mut Frame,
    state: &TuiState,
    pending: Option<&PendingArb>,
    _queue: Arc<ConfirmationQueue>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(65),
            Constraint::Percentage(35),
        ])
        .split(f.area());

    draw_log_pane(f, state, chunks[0]);
    draw_confirm_pane(f, state, pending, chunks[1]);
}

fn draw_log_pane(f: &mut Frame, state: &TuiState, area: Rect) {
    let items: Vec<ListItem> = state
        .log_buffer
        .iter()
        .map(|line| ListItem::new(line.as_str()))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Logs "));

    f.render_widget(list, area);
}

fn draw_confirm_pane(f: &mut Frame, state: &TuiState, pending: Option<&PendingArb>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirmation ");

    if let Some(arb) = pending {
        let inner = block.inner(area);
        f.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),  // Market info
                Constraint::Length(3),  // URLs
                Constraint::Length(3),  // Menu
            ])
            .split(inner);

        // Market info
        let arb_type_str = match arb.request.arb_type {
            crate::types::ArbType::PolyYesKalshiNo => "BUY Poly YES + BUY Kalshi NO",
            crate::types::ArbType::KalshiYesPolyNo => "BUY Kalshi YES + BUY Poly NO",
            crate::types::ArbType::PolyOnly => "BUY Poly YES + BUY Poly NO",
            crate::types::ArbType::KalshiOnly => "BUY Kalshi YES + BUY Kalshi NO",
        };

        let info_text = format!(
            "{}\n{}\nProfit: {}¢/contract | Size: {} contracts | Detected {}x",
            arb.pair.description,
            arb_type_str,
            arb.profit_cents(),
            arb.max_contracts(),
            arb.detection_count,
        );

        let info = Paragraph::new(info_text)
            .wrap(Wrap { trim: true });
        f.render_widget(info, chunks[0]);

        // URLs
        let urls = Paragraph::new(format!(
            "Kalshi: {}\nPoly:   {}",
            arb.kalshi_url, arb.poly_url
        ));
        f.render_widget(urls, chunks[1]);

        // Menu (or note input)
        if let Some(ref note) = state.note_input {
            let note_input = Paragraph::new(format!("Note: {}█", note))
                .style(Style::default().fg(Color::Yellow));
            f.render_widget(note_input, chunks[2]);
        } else {
            let menu_spans: Vec<Span> = MenuOption::all()
                .iter()
                .map(|opt| {
                    let style = if *opt == state.selected {
                        Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Span::styled(format!(" [{}] ", opt.label()), style)
                })
                .collect();

            let menu = Paragraph::new(Line::from(menu_spans));
            f.render_widget(menu, chunks[2]);
        }
    } else {
        let waiting = Paragraph::new("Waiting for opportunities...")
            .block(block);
        f.render_widget(waiting, area);
    }
}
```

**Step 2: Add module to main.rs**

In `controller/src/main.rs`, add after `mod confirm_queue;`:

```rust
mod confirm_tui;
```

**Step 3: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles without errors

**Step 4: Commit**

```bash
git add controller/src/confirm_tui.rs controller/src/main.rs
git commit -m "feat(confirm): add split-pane TUI for confirmation"
```

---

## Task 7: Integrate confirmation layer into main.rs

**Files:**
- Modify: `controller/src/main.rs`

**Step 1: Add confirmation channel and handler**

This is the most complex integration. In `controller/src/main.rs`, we need to:

1. Create a channel for arbs that need confirmation
2. Route arbs based on `requires_confirmation(league)`
3. Launch TUI when first confirmation-needed arb arrives
4. Process user decisions

Add imports at the top of main.rs:

```rust
use confirm_log::{ConfirmationLogger, ConfirmationRecord, ConfirmationStatus};
use confirm_queue::{ConfirmAction, ConfirmationQueue, PendingArb};
use confirm_tui::TuiState;
```

After the execution channel setup (~line 870), add confirmation channel:

```rust
// Confirmation mode setup
let confirm_enabled = config::any_league_requires_confirmation();
let (confirm_tx, mut confirm_rx) = mpsc::channel::<(FastExecutionRequest, Arc<MarketPair>)>(256);
let (tui_update_tx, tui_update_rx) = mpsc::channel::<()>(16);
let (tui_action_tx, mut tui_action_rx) = mpsc::channel::<ConfirmAction>(16);

let confirm_queue = Arc::new(ConfirmationQueue::new(state.clone(), tui_update_tx));
let tui_state = Arc::new(RwLock::new(TuiState::new()));
```

Create a helper function to route arbs (add before `async fn main()`):

```rust
/// Route an arb request to either confirmation queue or direct execution
async fn route_arb_request(
    req: FastExecutionRequest,
    state: &Arc<GlobalState>,
    exec_tx: &mpsc::Sender<FastExecutionRequest>,
    confirm_tx: &mpsc::Sender<(FastExecutionRequest, Arc<MarketPair>)>,
) {
    let market = match state.get_by_id(req.market_id) {
        Some(m) => m,
        None => return,
    };

    let pair = match market.pair() {
        Some(p) => p,
        None => return,
    };

    if config::requires_confirmation(&pair.league) {
        let _ = confirm_tx.try_send((req, pair));
    } else {
        let _ = exec_tx.try_send(req);
    }
}
```

Add the confirmation handler task after the execution loop setup:

```rust
// Confirmation handler task
if confirm_enabled {
    let confirm_queue_clone = confirm_queue.clone();
    let confirm_exec_tx = exec_tx.clone();
    let confirm_state = state.clone();
    let confirm_tui_state = tui_state.clone();

    tokio::spawn(async move {
        let mut logger = ConfirmationLogger::new().expect("Failed to create confirmation logger");
        let mut tui_handle: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            tokio::select! {
                // New arb needing confirmation
                Some((req, pair)) = confirm_rx.recv() => {
                    confirm_queue_clone.push(req, pair).await;

                    // Launch TUI if not already running and queue not empty
                    if tui_handle.is_none() && !confirm_queue_clone.is_empty().await {
                        let q = confirm_queue_clone.clone();
                        let s = confirm_tui_state.clone();
                        let tx = tui_action_tx.clone();
                        let rx = tui_update_rx; // Move receiver to TUI task

                        tui_handle = Some(tokio::spawn(async move {
                            if let Err(e) = confirm_tui::run_tui(q, s, rx, tx).await {
                                tracing::error!("TUI error: {}", e);
                            }
                        }));
                    }
                }

                // User action from TUI
                Some(action) = tui_action_rx.recv() => {
                    if let Some(arb) = confirm_queue_clone.pop_front().await {
                        let status = match &action {
                            ConfirmAction::Proceed => {
                                if confirm_queue_clone.validate_arb(&arb) {
                                    // Still valid, send to execution
                                    let _ = confirm_exec_tx.try_send(arb.request);
                                    ConfirmationStatus::Accepted
                                } else {
                                    ConfirmationStatus::AcceptedExpired
                                }
                            }
                            ConfirmAction::Reject { .. } => ConfirmationStatus::Rejected,
                            ConfirmAction::Blacklist { .. } => {
                                confirm_queue_clone.blacklist_market(arb.request.market_id).await;
                                ConfirmationStatus::Blacklisted
                            }
                        };

                        let note = match &action {
                            ConfirmAction::Reject { note } | ConfirmAction::Blacklist { note } => note.clone(),
                            _ => None,
                        };

                        // Log the decision
                        let record = ConfirmationRecord {
                            timestamp: chrono::Utc::now(),
                            status,
                            market_id: arb.request.market_id,
                            pair_id: arb.pair.pair_id.to_string(),
                            description: arb.pair.description.to_string(),
                            league: arb.pair.league.to_string(),
                            arb_type: arb.request.arb_type,
                            yes_price_cents: arb.request.yes_price,
                            no_price_cents: arb.request.no_price,
                            profit_cents: arb.profit_cents(),
                            max_contracts: arb.max_contracts(),
                            detection_count: arb.detection_count,
                            kalshi_url: arb.kalshi_url.clone(),
                            poly_url: arb.poly_url.clone(),
                            note,
                        };

                        if let Err(e) = logger.log(record) {
                            tracing::error!("Failed to log confirmation: {}", e);
                        }
                    }

                    // Check if TUI should close (queue empty)
                    if confirm_queue_clone.is_empty().await {
                        if let Some(handle) = tui_handle.take() {
                            handle.abort();
                        }
                    }
                }
            }
        }
    });
}
```

**Step 2: Update WebSocket handlers to use route_arb_request**

In the Polymarket and Kalshi WebSocket message handlers where they call `exec_tx.try_send(req)`, change to call `route_arb_request()` instead. This requires passing the confirm_tx channel to those handlers.

This is a larger refactor - the `send_arb_request` helper functions in `polymarket.rs` and `kalshi.rs` need to be updated.

**Step 3: Verify compiles**

Run: `cargo check -p controller`
Expected: Compiles (may have warnings)

**Step 4: Commit**

```bash
git add controller/src/main.rs
git commit -m "feat(confirm): integrate confirmation layer into main execution flow"
```

---

## Task 8: Update polymarket.rs to support confirmation routing

**Files:**
- Modify: `controller/src/polymarket.rs`

**Step 1: Update run_ws signature to accept confirm channel**

The `run_ws` function needs an additional parameter for routing arbs that need confirmation. Update the function signature and internal logic to call `route_arb_request` or equivalent.

This task depends on the specific implementation in `polymarket.rs`. The key change is replacing direct `exec_tx.try_send(req)` calls with a routing function that checks `requires_confirmation()`.

**Step 2: Verify compiles**

Run: `cargo check -p controller`

**Step 3: Commit**

```bash
git add controller/src/polymarket.rs
git commit -m "feat(confirm): add confirmation routing to Polymarket WebSocket handler"
```

---

## Task 9: Update kalshi.rs to support confirmation routing

**Files:**
- Modify: `controller/src/kalshi.rs`

**Step 1: Update run_ws signature to accept confirm channel**

Same pattern as polymarket.rs - update to route arbs through confirmation check.

**Step 2: Verify compiles**

Run: `cargo check -p controller`

**Step 3: Commit**

```bash
git add controller/src/kalshi.rs
git commit -m "feat(confirm): add confirmation routing to Kalshi WebSocket handler"
```

---

## Task 10: Add --confirm-mode-skip CLI argument

**Files:**
- Modify: `controller/src/main.rs` (CLI arg parsing section)
- Modify: `controller/src/config.rs`

**Step 1: Parse CLI arg and set env var**

In main.rs, near the other CLI argument parsing (~line 111-120):

```rust
// Confirm mode skip from CLI
if let Some(skip) = cli_arg_value(&args, "--confirm-mode-skip") {
    std::env::set_var("CONFIRM_MODE_SKIP", &skip);
}
```

**Step 2: Update config.rs to not cache (allow CLI override)**

The `confirm_mode_skip_leagues()` function should NOT use OnceLock caching so CLI can override env vars:

```rust
pub fn confirm_mode_skip_leagues() -> HashSet<String> {
    std::env::var("CONFIRM_MODE_SKIP")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|l| l.trim().to_lowercase()).collect())
        .unwrap_or_default()
}
```

**Step 3: Verify compiles**

Run: `cargo check -p controller`

**Step 4: Commit**

```bash
git add controller/src/main.rs controller/src/config.rs
git commit -m "feat(confirm): add --confirm-mode-skip CLI argument"
```

---

## Task 11: Add integration test for confirmation flow

**Files:**
- Create: `controller/tests/confirm_mode_test.rs`

**Step 1: Create basic integration test**

```rust
//! Integration tests for confirm mode

use arb_bot::config::{requires_confirmation, confirm_mode_skip_leagues};

#[test]
fn test_requires_confirmation_default() {
    // All leagues should require confirmation by default
    assert!(requires_confirmation("nba"));
    assert!(requires_confirmation("epl"));
    assert!(requires_confirmation("unknown_league"));
}

#[test]
fn test_confirm_mode_skip() {
    std::env::set_var("CONFIRM_MODE_SKIP", "nba,nfl");

    // Skipped leagues should not require confirmation
    assert!(!requires_confirmation("nba"));
    assert!(!requires_confirmation("nfl"));

    // Other leagues still require confirmation
    assert!(requires_confirmation("epl"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}
```

**Step 2: Run tests**

Run: `cargo test -p controller confirm_mode`
Expected: All tests pass

**Step 3: Commit**

```bash
git add controller/tests/confirm_mode_test.rs
git commit -m "test(confirm): add integration tests for confirm mode"
```

---

## Task 12: Update TODO.md

**Files:**
- Modify: `TODO.md`

**Step 1: Mark confirm mode as complete**

Update the TODO.md to mark the confirm mode item as done:

```markdown
- [x] **Controller confirm mode** (PR #XX)
  - [x] Add a mode that requires explicit confirmation before dispatching execution legs
  - [x] Ensure it works for cross-platform + same-platform arbs (and remote-trader routing)
```

**Step 2: Commit**

```bash
git add TODO.md
git commit -m "docs: mark confirm mode as complete in TODO.md"
```

---

## Task 13: Final verification

**Step 1: Run full test suite**

Run: `cargo test -p controller`
Expected: All tests pass

**Step 2: Build release**

Run: `cargo build -p controller --release`
Expected: Compiles without errors

**Step 3: Manual smoke test**

Run: `CONFIRM_MODE_SKIP= DRY_RUN=1 cargo run -p controller --release`

Expected behavior:
- Bot starts normally
- When first arb is detected for a confirm-mode league, TUI launches
- Can navigate menu with arrow keys
- Can approve/reject arbs
- TUI closes when queue empties
- Logs return to normal scrolling

**Step 4: Test with skip list**

Run: `CONFIRM_MODE_SKIP=nba,nfl DRY_RUN=1 cargo run -p controller --release`

Expected: NBA and NFL arbs auto-execute, other leagues require confirmation

---
