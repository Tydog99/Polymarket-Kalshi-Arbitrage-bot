//! Split-pane TUI for arbitrage confirmation.
//!
//! Upper 2/3: scrolling log buffer
//! Lower 1/3: confirmation panel with pending arb details

use std::io;
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
    /// Saved note to attach to action (persists after exiting note mode)
    pub saved_note: Option<String>,
    /// Whether TUI is active
    pub active: bool,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            log_buffer: VecDeque::with_capacity(MAX_LOG_LINES),
            selected: MenuOption::Proceed,
            note_input: None,
            saved_note: None,
            active: false,
        }
    }
}

impl TuiState {
    pub fn new() -> Self {
        Self::default()
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
    mut log_rx: mpsc::Receiver<String>,
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

        // Drain log channel into buffer
        {
            let mut tui_state = state.write().await;
            while let Ok(line) = log_rx.try_recv() {
                tui_state.add_log(line);
            }
        }

        // Get current pending arb
        let pending = queue.front().await;

        // Render
        {
            let tui_state = state.read().await;
            terminal.draw(|f| {
                draw_ui(f, &tui_state, pending.as_ref());
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

    // Dump captured logs to main terminal
    {
        let tui_state = state.read().await;
        if !tui_state.log_buffer.is_empty() {
            println!("\n--- Logs from confirmation session ---");
            for line in &tui_state.log_buffer {
                println!("{}", line);
            }
            println!("--- End of confirmation session logs ---\n");
        }
    }

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
                // Save note and return to menu (don't submit action yet)
                tui_state.saved_note = if note.is_empty() { None } else { Some(note.clone()) };
                tui_state.note_input = None;
            }
            KeyCode::Esc => {
                // Cancel note entry (don't save)
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
                let note = tui_state.saved_note.take(); // Take and clear saved note
                let action = match tui_state.selected {
                    MenuOption::Proceed => ConfirmAction::Proceed,
                    MenuOption::Reject => ConfirmAction::Reject { note },
                    MenuOption::Blacklist => ConfirmAction::Blacklist { note },
                };
                return TuiResult::Action(action);
            }
        }
        KeyCode::Char('n') => {
            // Enter note mode (pre-fill with existing saved note if any)
            tui_state.note_input = Some(tui_state.saved_note.clone().unwrap_or_default());
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
    let block = Block::default().borders(Borders::ALL).title(" Logs ");

    // Calculate visible height (subtract 2 for top/bottom borders)
    let visible_height = area.height.saturating_sub(2) as usize;

    // Get only the most recent logs that fit in the visible area (auto-scroll to bottom)
    let log_count = state.log_buffer.len();
    let skip = log_count.saturating_sub(visible_height);

    let items: Vec<ListItem> = state
        .log_buffer
        .iter()
        .skip(skip)
        .map(|line| ListItem::new(line.as_str()))
        .collect();

    let list = List::new(items).block(block);

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
                Constraint::Length(1),  // Help
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
            "{}\n{}\nProfit: {}c/contract | Size: {} contracts | Detected {}x",
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
            let note_input = Paragraph::new(format!("Note: {}_", note))
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

            // Add note indicator if note is saved
            let mut spans = menu_spans;
            if state.saved_note.is_some() {
                spans.push(Span::styled("  [note ✓]", Style::default().fg(Color::Green)));
            }

            let menu = Paragraph::new(Line::from(spans));
            f.render_widget(menu, chunks[2]);
        }

        // Help line
        let help_text = if state.saved_note.is_some() {
            "← → navigate | Enter confirm | n edit note | q quit"
        } else {
            "← → navigate | Enter confirm | n add note | q quit"
        };
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(help, chunks[3]);
    } else {
        let waiting = Paragraph::new("Waiting for opportunities...")
            .block(block);
        f.render_widget(waiting, area);
    }
}
