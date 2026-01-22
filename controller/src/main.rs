//! Prediction Market Arbitrage Trading System
//!
//! A high-performance, production-ready arbitrage trading system for cross-platform
//! prediction markets. This system monitors price discrepancies between Kalshi and
//! Polymarket, executing risk-free arbitrage opportunities in real-time.
//!
//! ## Strategy
//!
//! The core arbitrage strategy exploits the fundamental property of prediction markets:
//! YES + NO = $1.00 (guaranteed). Arbitrage opportunities exist when:
//!
//! ```
//! Best YES ask (Platform A) + Best NO ask (Platform B) < $1.00
//! ```
//!
//! ## Architecture
//!
//! - **Real-time price monitoring** via WebSocket connections to both platforms
//! - **Lock-free orderbook cache** using atomic operations for zero-copy updates
//! - **SIMD-accelerated arbitrage detection** for sub-millisecond latency
//! - **Concurrent order execution** with automatic position reconciliation
//! - **Circuit breaker protection** with configurable risk limits
//! - **Market discovery system** with intelligent caching and incremental updates

mod cache;
mod circuit_breaker;
mod config;
mod confirm_log;
mod confirm_queue;
mod confirm_tui;
mod discovery;
mod execution;
mod kalshi;
mod paths;
mod poly_executor;
mod polymarket;
mod polymarket_clob;
mod position_tracker;
mod remote_execution;
mod remote_protocol;
mod remote_trader;
mod types;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tailscale::beacon::BeaconSender;
use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use cache::TeamCache;
use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use config::{ARB_THRESHOLD, enabled_leagues, get_league_configs, parse_controller_platforms, WS_RECONNECT_DELAY_SECS};
use confirm_log::{ConfirmationLogger, ConfirmationRecord, ConfirmationStatus};
use confirm_queue::{ConfirmAction, ConfirmationQueue};
use confirm_tui::TuiState;
use discovery::DiscoveryClient;
use execution::{ExecutionEngine, NanoClock, create_execution_channel, run_execution_loop};
use kalshi::{KalshiConfig, KalshiApiClient};
use polymarket_clob::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};
use position_tracker::{PositionTracker, create_position_channel, position_writer_loop};
use crate::remote_execution::{HybridExecutor, run_hybrid_execution_loop};
use crate::remote_protocol::Platform as WsPlatform;
use crate::remote_trader::RemoteTraderServer;
use trading::execution::Platform as TradingPlatform;
use types::{FastExecutionRequest, GlobalState, MarketPair, MarketType, PriceCents};

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

fn cli_arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn cli_has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn fmt_age(now_ms: u64, then_ms: u64) -> String {
    if then_ms == 0 || then_ms > now_ms {
        return "--".to_string();
    }
    let delta_ms = now_ms - then_ms;
    if delta_ms < 1_000 {
        format!("{}ms", delta_ms)
    } else if delta_ms < 60_000 {
        format!("{:.1}s", (delta_ms as f64) / 1000.0)
    } else if delta_ms < 3_600_000 {
        format!("{:.1}m", (delta_ms as f64) / 60_000.0)
    } else {
        format!("{:.1}h", (delta_ms as f64) / 3_600_000.0)
    }
}

fn fmt_unix_ms_hhmmss(unix_ms: u64) -> String {
    if unix_ms == 0 {
        return "--:--:--".to_string();
    }
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(unix_ms as i64)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
}

/// Print a summary table of discovery results
fn print_discovery_summary(result: &types::DiscoveryResult) {
    use std::collections::BTreeSet;

    // Collect all unique leagues from stats
    let leagues: BTreeSet<_> = result.stats.by_league_type
        .keys()
        .map(|(league, _)| league.clone())
        .collect();

    if leagues.is_empty() {
        return;
    }

    let market_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
    let type_headers = ["Moneyline", "Spread", "Total", "BTTS"];

    // Calculate column widths
    let league_width = leagues.iter().map(|l| l.len()).max().unwrap_or(6).max(6);

    // Print header
    info!("┌{:─<width$}┬{:─<48}┬{:─<8}┬{:─<8}┐",
          "", "", "", "", width = league_width + 2);
    info!("│{:^width$}│{:^48}│{:^8}│{:^8}│",
          "DISCOVERY SUMMARY", "", "", "", width = league_width + 2);
    info!("├{:─<width$}┼{:─<11}─{:─<11}─{:─<11}─{:─<11}┼{:─<8}┼{:─<8}┤",
          "", "", "", "", "", "", "", width = league_width + 2);
    info!("│ {:width$} │ {:^9} {:^9} {:^9} {:^9}   │ {:^6} │ {:^6} │",
          "League", type_headers[0], type_headers[1], type_headers[2], type_headers[3], "Kalshi", "Match",
          width = league_width);
    info!("├{:─<width$}┼{:─<11}─{:─<11}─{:─<11}─{:─<11}┼{:─<8}┼{:─<8}┤",
          "", "", "", "", "", "", "", width = league_width + 2);

    // Print each league row
    let mut total_kalshi = 0usize;
    let mut total_matched = 0usize;

    for league in &leagues {
        let mut row_kalshi = 0usize;
        let mut row_matched = 0usize;
        let mut type_strs: Vec<String> = Vec::new();

        for mt in &market_types {
            if let Some(&(kalshi, matched)) = result.stats.by_league_type.get(&(league.clone(), *mt)) {
                type_strs.push(format!("{}/{}", matched, kalshi));
                row_kalshi += kalshi;
                row_matched += matched;
            } else {
                type_strs.push("-".to_string());
            }
        }

        info!("│ {:width$} │ {:^9} {:^9} {:^9} {:^9}   │ {:^6} │ {:^6} │",
              league,
              type_strs[0], type_strs[1], type_strs[2], type_strs[3],
              row_kalshi, row_matched,
              width = league_width);

        total_kalshi += row_kalshi;
        total_matched += row_matched;
    }

    // Print footer with totals
    info!("├{:─<width$}┼{:─<11}─{:─<11}─{:─<11}─{:─<11}┼{:─<8}┼{:─<8}┤",
          "", "", "", "", "", "", "", width = league_width + 2);
    info!("│ {:width$} │ {:^9} {:^9} {:^9} {:^9}   │ {:^6} │ {:^6} │",
          "TOTAL", "", "", "", "", total_kalshi, total_matched, width = league_width);
    info!("└{:─<width$}┴{:─<48}┴{:─<8}┴{:─<8}┘",
          "", "", "", "", width = league_width + 2);
}

/// Background task that periodically discovers new markets
async fn discovery_refresh_task(
    discovery: Arc<DiscoveryClient>,
    state: Arc<GlobalState>,
    shutdown_tx: watch::Sender<bool>,
    interval_mins: u64,
    leagues: Vec<String>,
    tui_state: Arc<tokio::sync::RwLock<crate::confirm_tui::TuiState>>,
    log_tx: tokio::sync::mpsc::Sender<String>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Helper to check TUI state and route logs appropriately
    let log_line = |msg: String, tui_active: bool| {
        if tui_active {
            let _ = log_tx.try_send(msg);
        } else {
            info!("{}", msg);
        }
    };

    let tui_active = tui_state.read().await.active;
    if interval_mins == 0 {
        log_line("[DISCOVERY] Runtime discovery disabled (DISCOVERY_INTERVAL_MINS=0)".to_string(), tui_active);
        return;
    }

    log_line(format!("[DISCOVERY] Runtime discovery enabled (interval: {}m)", interval_mins), tui_active);

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_mins * 60));
    interval.tick().await; // Skip immediate first tick

    // Track last discovery timestamp
    let mut last_discovery_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    loop {
        interval.tick().await;

        let tui_active = tui_state.read().await.active;
        log_line("[DISCOVERY] Running scheduled discovery...".to_string(), tui_active);

        // Build set of known tickers
        let known_tickers: HashSet<String> = state.markets.iter()
            .take(state.market_count())
            .filter_map(|m| m.pair())
            .map(|p| p.kalshi_market_ticker.to_string())
            .collect();

        // Discover new markets since last check
        let league_refs: Vec<&str> = leagues.iter().map(|s| s.as_str()).collect();
        let result = discovery
            .discover_since(last_discovery_ts, &known_tickers, &league_refs)
            .await;

        // Update timestamp for next iteration
        last_discovery_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Log errors but continue
        let tui_active = tui_state.read().await.active;
        for err in &result.errors {
            log_line(format!("[DISCOVERY] {}", err), tui_active);
        }

        if result.pairs.is_empty() {
            log_line("[DISCOVERY] No new markets found".to_string(), tui_active);
            continue;
        }

        // Log new markets with highlighted formatting
        log_line(format!("[DISCOVERY] NEW MARKETS DISCOVERED: {}", result.pairs.len()), tui_active);
        for pair in &result.pairs {
            log_line(format!("[DISCOVERY]   -> {} | {} | {}",
                pair.league, pair.description, pair.kalshi_market_ticker), tui_active);
        }

        // Add new pairs to global state (thread-safe via interior mutability)
        let mut added_count = 0;
        for pair in result.pairs {
            if let Some(market_id) = state.add_pair(pair) {
                added_count += 1;
                log_line(format!("[DISCOVERY] Added market_id {} to state", market_id), tui_active);
            } else {
                log_line("[DISCOVERY] Failed to add pair - state full (MAX_MARKETS reached)".to_string(), tui_active);
            }
        }
        log_line(format!("[DISCOVERY] Added {} new markets to state (total: {})", added_count, state.market_count()), tui_active);

        // Signal WebSockets to reconnect with updated subscriptions
        log_line(format!("[DISCOVERY] Signaling WebSocket reconnect for {} new markets...", added_count), tui_active);
        if shutdown_tx.send(true).is_err() {
            log_line("[DISCOVERY] Failed to signal WebSocket reconnect - receivers dropped".to_string(), tui_active);
        }

        // Give WebSockets time to shut down gracefully
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Reset shutdown signal for next cycle
        if shutdown_tx.send(false).is_err() {
            log_line("[DISCOVERY] Failed to reset shutdown signal - receivers dropped".to_string(), tui_active);
        }
    }
}

fn parse_bool_env(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v.to_lowercase() == "true" || v.to_lowercase() == "yes")
        .unwrap_or(false)
}

/// Deduplicate market names that have repeated prefixes.
/// e.g., "Union Berlin vs Dortmund - Union Berlin vs Dortmund Winner?"
///    -> "Union Berlin vs Dortmund Winner?"
fn deduplicate_market_name(description: &str) -> String {
    if let Some(sep_pos) = description.find(" - ") {
        let prefix = &description[..sep_pos];
        let suffix = &description[sep_pos + 3..];
        // If suffix starts with prefix, keep only suffix
        if suffix.starts_with(prefix) {
            return suffix.to_string();
        }
    }
    description.to_string()
}

/// Strip redundant market type text from description when displayed under a type header.
/// e.g., "Leeds at Everton: Spreads - Everton wins by..." -> "Leeds at Everton - Everton wins by..."
///       "Leeds at Everton: Totals 2.5" -> "Leeds at Everton 2.5"
///       "Leeds at Everton: Both Teams to Score" -> "Leeds at Everton"
///       "Leeds vs Everton Winner?" -> "Leeds vs Everton"
fn strip_market_type_suffix(description: &str, market_type: &types::MarketType) -> String {
    match market_type {
        types::MarketType::Moneyline => {
            description
                .replace(" Winner?", "")
                .replace(" Winner", "")
                .replace("(Tie)", "(Draw)")
                .replace("(tie)", "(Draw)")
        }
        types::MarketType::Spread => {
            description
                .replace(": Spreads - ", " - ")
                .replace(": Spreads ", " ")
                .replace(" wins by over ", " by ")
                .replace(" goals?", "")
        }
        types::MarketType::Total => {
            description
                .replace(": Totals ", " ")
                .replace(": Totals", "")
        }
        types::MarketType::Btts => {
            description
                .replace(": Both Teams to Score", "")
                .replace(" Both Teams to Score", "")
        }
    }
}

fn parse_ws_platforms() -> Vec<WsPlatform> {
    // Default to both platforms.
    let raw =
        std::env::var("TRADER_PLATFORMS").unwrap_or_else(|_| "kalshi,polymarket".to_string());
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter_map(|s| match s.as_str() {
            "kalshi" => Some(WsPlatform::Kalshi),
            "polymarket" | "poly" => Some(WsPlatform::Polymarket),
            _ => None,
        })
        .collect()
}

/// Maximum number of log files to retain in the logs directory
const MAX_LOG_FILES: usize = 10;

/// Initialize logging with both console and file output.
///
/// Returns the log file path and a [`WorkerGuard`] that **must be kept alive** for the
/// entire duration of the program. The guard ensures buffered logs are flushed on shutdown.
/// Dropping the guard early will cause log loss.
///
/// # Usage
/// ```ignore
/// let (log_path, _log_guard) = init_logging();
/// // _log_guard lives until main() exits, ensuring all logs are flushed
/// ```
fn init_logging() -> (PathBuf, WorkerGuard) {
    use chrono::Local;

    // Create logs directory in project root (same level as Cargo.toml)
    let logs_dir = PathBuf::from(".logs");
    std::fs::create_dir_all(&logs_dir).expect("Failed to create logs directory");

    // Generate timestamped filename: controller-2026-01-17-143052.log
    let timestamp = Local::now().format("%Y-%m-%d-%H%M%S");
    let log_filename = format!("controller-{}.log", timestamp);
    let log_path = logs_dir.join(&log_filename);

    // Get absolute path for logging (canonicalize requires file to exist, so we build it manually)
    let absolute_log_path = std::env::current_dir()
        .map(|cwd| cwd.join(&log_path))
        .unwrap_or_else(|_| log_path.clone());

    // Clean up old log files, keeping only the most recent MAX_LOG_FILES
    cleanup_old_logs(&logs_dir, MAX_LOG_FILES);

    // Create non-blocking file appender
    let file_appender = tracing_appender::rolling::never(&logs_dir, &log_filename);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Build env filter
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("info")
            .add_directive("controller=info".parse().unwrap())
            .add_directive("arb_bot=info".parse().unwrap())
    });

    // Create timer for consistent formatting
    let timer = tracing_subscriber::fmt::time::ChronoLocal::new("[%H:%M:%S]".to_string());

    // Console layer with color support
    let console_layer = tracing_subscriber::fmt::layer()
        .with_timer(timer.clone())
        .with_ansi(true);

    // File layer without ANSI codes
    let file_layer = tracing_subscriber::fmt::layer()
        .with_timer(timer)
        .with_ansi(false)
        .with_writer(non_blocking);

    // Combine layers
    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    (absolute_log_path, guard)
}

/// Remove old log files, keeping only the most recent `keep_count` files.
fn cleanup_old_logs(logs_dir: &PathBuf, keep_count: usize) {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return;
    };

    // Collect log files with their modification times
    let mut log_files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "log")
                .unwrap_or(false)
        })
        .filter_map(|e| {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (e.path(), t))
        })
        .collect();

    // Sort by modification time (newest first)
    log_files.sort_by(|a, b| b.1.cmp(&a.1));

    // Remove files beyond keep_count
    for (path, _) in log_files.into_iter().skip(keep_count) {
        let _ = std::fs::remove_file(&path);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging with both console and file output
    // The guard must be kept alive for the duration of the program to ensure logs are flushed
    let (log_file_path, _log_guard) = init_logging();
    info!("📝 Logging to: {}", log_file_path.display());

    // Load environment variables from `.env` (supports workspace-root `.env`)
    paths::load_dotenv();

    // Initialize HTTP capture session if CAPTURE_DIR is set
    // Must be done early, before any HTTP clients are created
    match trading::capture::init_capture_session() {
        Ok(Some(session)) => {
            info!(
                "📼 Capture mode active: {} (filter: {})",
                session.session_dir.display(),
                session.filter
            );
        }
        Ok(None) => {
            // Capture disabled - nothing to log
        }
        Err(e) => {
            error!("Failed to initialize capture session: {}", e);
            error!("Check that CAPTURE_DIR points to a writable directory");
            return Err(e.into());
        }
    }

    // === Remote smoke test mode ===
    // Runs only the controller-hosted WS server, waits for a trader to connect,
    // sends a single synthetic execute message, then exits.
    if parse_bool_env("REMOTE_SMOKE_TEST") {
        let dry_run = std::env::var("DRY_RUN").map(|v| v == "1" || v == "true").unwrap_or(true);
        let bind: std::net::SocketAddr = std::env::var("REMOTE_TRADER_BIND")
            .unwrap_or_else(|_| "127.0.0.1:9001".to_string())
            .parse()
            .context("REMOTE_TRADER_BIND must be a SocketAddr, e.g. 127.0.0.1:9001")?;

        let platforms = parse_ws_platforms();
        let remote_server = RemoteTraderServer::new(bind, platforms, dry_run);
        let trader_router = remote_server.router();
        tokio::spawn(async move {
            if let Err(e) = remote_server.run().await {
                error!("[REMOTE] server error: {}", e);
            }
        });

        info!("[REMOTE] Waiting for trader connection...");
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
        let mut connected_platform: Option<crate::remote_protocol::Platform> = None;
        loop {
            for p in parse_ws_platforms() {
                if trader_router.is_connected(p).await {
                    connected_platform = Some(p);
                    break;
                }
            }
            if connected_platform.is_some() { break; }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("Timed out waiting for trader to connect");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }

        let platform = connected_platform.unwrap();
        warn!("[REMOTE] Trader connected ({:?}); sending synthetic execute_leg", platform);
        let leg = crate::remote_protocol::IncomingMessage::ExecuteLeg {
            market_id: 0,
            leg_id: "smoke-leg-1".to_string(),
            platform,
            action: crate::remote_protocol::OrderAction::Buy,
            side: crate::remote_protocol::OutcomeSide::Yes,
            price: 40,
            contracts: 1,
            kalshi_market_ticker: if platform == crate::remote_protocol::Platform::Kalshi {
                Some("KXSMOKE-YES".to_string())
            } else {
                None
            },
            poly_token: if platform == crate::remote_protocol::Platform::Polymarket {
                Some("0x_smoke_yes".to_string())
            } else {
                None
            },
            pair_id: Some("smoke-test".to_string()),
            description: Some("Smoke Test Market".to_string()),
        };
        trader_router.send(platform, leg).await?;

        info!("[REMOTE] Sent execute; sleeping briefly then exiting");
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        return Ok(());
    }

    info!("🚀 Prediction Market Arbitrage System v2.0");
    info!("   Profit threshold: <{:.1}¢ ({:.1}% minimum profit)",
          ARB_THRESHOLD * 100.0, (1.0 - ARB_THRESHOLD) * 100.0);

    // --- CLI overrides (faster iteration) ---
    // Examples:
    //   cargo run -p controller -- --leagues nba
    //   cargo run -p controller -- --leagues nba,nfl --pairing-debug --pairing-debug-limit 50
    //   cargo run -p controller -- --verbose-heartbeat --heartbeat-interval 5
    let args: Vec<String> = std::env::args().skip(1).collect();
    if cli_has_flag(&args, "--verbose-heartbeat") {
        std::env::set_var("VERBOSE_HEARTBEAT", "1");
    }
    if let Some(v) = cli_arg_value(&args, "--heartbeat-interval") {
        std::env::set_var("HEARTBEAT_INTERVAL_SECS", v);
    }
    if cli_has_flag(&args, "--pairing-debug") {
        std::env::set_var("PAIRING_DEBUG", "1");
    }
    if let Some(v) = cli_arg_value(&args, "--pairing-debug-limit") {
        std::env::set_var("PAIRING_DEBUG_LIMIT", v);
    }
    if cli_has_flag(&args, "--verbose-heartbeat") {
        std::env::set_var("VERBOSE_HEARTBEAT", "1");
    }
    if let Some(v) = cli_arg_value(&args, "--heartbeat-interval") {
        std::env::set_var("HEARTBEAT_INTERVAL_SECS", v);
    }
    if let Some(skip) = cli_arg_value(&args, "--confirm-mode-skip") {
        std::env::set_var("CONFIRM_MODE_SKIP", &skip);
    }

    // Build league list for discovery from CLI/env.
    // - `--leagues nba,nfl` overrides env.
    // - If nothing is specified, we monitor all supported leagues.
    let cli_leagues: Option<Vec<String>> = cli_arg_value(&args, "--leagues").map(|v| {
        v.split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let env_leagues: Vec<String> = enabled_leagues().to_vec();

    let leagues_owned: Vec<String> = match cli_leagues {
        Some(v) if !v.is_empty() => v,
        _ if !env_leagues.is_empty() => env_leagues,
        _ => get_league_configs()
            .into_iter()
            .map(|c| c.league_code.to_string())
            .collect(),
    };

    let leagues: Vec<&str> = leagues_owned.iter().map(|s| s.as_str()).collect();
    info!("   Monitored leagues: {:?}", leagues);

    // Parse --market-type filter (e.g., --market-type moneyline)
    let market_type_filter: Option<MarketType> = cli_arg_value(&args, "--market-type")
        .and_then(|s| match s.to_lowercase().as_str() {
            "moneyline" => Some(MarketType::Moneyline),
            "spread" => Some(MarketType::Spread),
            "total" => Some(MarketType::Total),
            "btts" => Some(MarketType::Btts),
            _ => {
                warn!("Unknown market type '{}', ignoring filter", s);
                None
            }
        });
    if let Some(mt) = &market_type_filter {
        info!("   Market type filter: {:?}", mt);
    }

    // Check for dry run mode
    let dry_run = std::env::var("DRY_RUN").map(|v| v == "1" || v == "true").unwrap_or(true);
    if dry_run {
        info!("   Mode: DRY RUN (set DRY_RUN=0 to execute)");
    } else {
        warn!("   Mode: LIVE EXECUTION");
    }

    // Parse which platforms the controller can execute locally
    let local_platforms = parse_controller_platforms();
    if local_platforms.is_empty() {
        info!("   Mode: PURE ROUTER (no local execution)");
    } else {
        info!("   Local platforms: {:?}", local_platforms);
    }

    // DISCOVERY_ONLY=1 will run market discovery, print results, and exit.
    // This is useful for validating credentials + cache paths without running websockets/execution.
    let discovery_only = std::env::var("DISCOVERY_ONLY")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    // Load Kalshi credentials
    let kalshi_config = KalshiConfig::from_env()?;
    info!("[KALSHI] API key loaded");

    // Load team code mapping cache
    let team_cache = TeamCache::load();
    info!("📂 Loaded {} team code mappings", team_cache.len());

    // Run discovery (with caching support)
    let force_discovery = std::env::var("FORCE_DISCOVERY")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    info!("🔍 Market discovery{}...",
          if force_discovery { " (forced refresh)" } else { "" });

    let discovery = DiscoveryClient::new(
        KalshiApiClient::new(KalshiConfig::from_env()?),
        team_cache
    );

    let result = if force_discovery {
        discovery.discover_all_force(&leagues, market_type_filter).await
    } else {
        discovery.discover_all(&leagues, market_type_filter).await
    };

    info!("📊 Market discovery complete:");
    print_discovery_summary(&result);
    info!("   - Matched market pairs: {}", result.pairs.len());

    if !result.errors.is_empty() {
        for err in &result.errors {
            warn!("   ⚠️ {}", err);
        }
    }

    if result.pairs.is_empty() {
        error!("No market pairs found!");
        return Ok(());
    }

    // Display discovered market pairs
    info!("📋 Discovered market pairs:");
    for pair in &result.pairs {
        info!("   ✅ {} | {} | Kalshi: {}",
              pair.description,
              pair.market_type,
              pair.kalshi_market_ticker);
    }

    if discovery_only {
        info!("✅ DISCOVERY_ONLY enabled; exiting after discovery.");
        return Ok(());
    }

    // Load Polymarket credentials
    let poly_private_key = std::env::var("POLY_PRIVATE_KEY")
        .context("POLY_PRIVATE_KEY not set")?;
    let poly_funder = std::env::var("POLY_FUNDER")
        .context("POLY_FUNDER not set (your wallet address)")?;

    // Create async Polymarket client and derive API credentials
    info!("[POLYMARKET] Creating async client and deriving API credentials...");
    let poly_async_client = PolymarketAsyncClient::new(
        POLY_CLOB_HOST,
        POLYGON_CHAIN_ID,
        &poly_private_key,
        &poly_funder,
    )?;
    let api_creds = poly_async_client.derive_api_key(0).await?;
    let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;
    let poly_async = Arc::new(SharedAsyncClient::new(poly_async_client, prepared_creds, POLYGON_CHAIN_ID));

    // Load neg_risk cache from Python script output
    let neg_risk_cache_path = paths::resolve_user_path(".clob_market_cache.json");
    match poly_async.load_cache(&neg_risk_cache_path.to_string_lossy()) {
        Ok(count) => info!("[POLYMARKET] Loaded {} neg_risk entries from cache", count),
        Err(e) => warn!("[POLYMARKET] Could not load neg_risk cache: {}", e),
    }

    info!("[POLYMARKET] Client ready for {}", &poly_funder[..10]);

    // Create Kalshi API client
    let kalshi_api = Arc::new(KalshiApiClient::new(kalshi_config));

    // Build global state
    let state = Arc::new({
        let s = GlobalState::new();
        for pair in result.pairs {
            s.add_pair(pair);
        }
        info!("📡 Global state initialized: tracking {} markets", s.market_count());
        s
    });

    // Initialize execution infrastructure
    let (exec_tx, exec_rx) = create_execution_channel();
    let circuit_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::from_env()));

    // Confirmation mode setup
    let confirm_enabled = config::any_league_requires_confirmation();
    let (confirm_tx, mut confirm_rx) = tokio::sync::mpsc::channel::<(FastExecutionRequest, Arc<MarketPair>)>(256);
    let (tui_update_tx, tui_update_rx) = tokio::sync::mpsc::channel::<()>(16);
    let (tui_action_tx, mut tui_action_rx) = tokio::sync::mpsc::channel::<ConfirmAction>(16);
    let (tui_log_tx, tui_log_rx) = tokio::sync::mpsc::channel::<String>(1024);

    let confirm_queue = Arc::new(ConfirmationQueue::new(state.clone(), tui_update_tx));
    let tui_state = Arc::new(RwLock::new(TuiState::new()));

    if confirm_enabled {
        info!("   Confirmation mode: ENABLED (some leagues require manual approval)");
    }

    // Create shutdown channel for WebSocket reconnection
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let position_tracker = Arc::new(RwLock::new(PositionTracker::new()));
    let (position_channel, position_rx) = create_position_channel();

    tokio::spawn(position_writer_loop(position_rx, position_tracker));

    let threshold_cents: PriceCents = ((ARB_THRESHOLD * 100.0).round() as u16).max(1);
    info!("   Execution threshold: {} cents", threshold_cents);

    // Shared clock for latency measurement across all components
    let clock = Arc::new(NanoClock::new());

    // Remote trader mode: controller hosts a WS server and forwards executions.
    // Set REMOTE_TRADER_BIND (e.g. "0.0.0.0:9001") to enable.
    let remote_bind = std::env::var("REMOTE_TRADER_BIND").ok();
    let remote_mode = remote_bind.is_some() || parse_bool_env("REMOTE_TRADER");

    let exec_handle = if remote_mode {
        let bind = remote_bind
            .unwrap_or_else(|| "0.0.0.0:9001".to_string())
            .parse()
            .context("REMOTE_TRADER_BIND must be a SocketAddr, e.g. 0.0.0.0:9001")?;
        let platforms = parse_ws_platforms();

        let remote_server = RemoteTraderServer::new(bind, platforms, dry_run);
        let trader_router = remote_server.router();
        tokio::spawn(async move {
            if let Err(e) = remote_server.run().await {
                error!("[REMOTE] server error: {}", e);
            }
        });

        // Start Tailscale beacon sender if Tailscale is available
        let beacon_cancel = CancellationToken::new();
        match tailscale::verify::verify() {
            Ok(ts_status) => {
                info!("[BEACON] Tailscale connected as {}", ts_status.self_ip);
                if !ts_status.peers.is_empty() {
                    let ts_config = tailscale::Config::load().unwrap_or_default();
                    info!(
                        "[BEACON] Starting beacon to {} peers (port {}, ws_port {})",
                        ts_status.peers.len(),
                        ts_config.beacon_port,
                        ts_config.ws_port
                    );
                    let beacon_cancel_clone = beacon_cancel.clone();
                    tokio::spawn(async move {
                        match BeaconSender::new(ts_status.peers, ts_config.beacon_port, ts_config.ws_port).await {
                            Ok(sender) => sender.run(beacon_cancel_clone).await,
                            Err(e) => error!("[BEACON] Failed to create beacon sender: {}", e),
                        }
                    });
                } else {
                    warn!("[BEACON] No Tailscale peers found - beacon disabled");
                    warn!("[BEACON] Ensure the trader machine has joined your Tailnet");
                }
            }
            Err(e) => {
                warn!("[BEACON] Tailscale not available: {}", e);
                warn!("[BEACON] To enable auto-discovery, set up Tailscale:");
                warn!("[BEACON]   1. Install: brew install tailscale");
                warn!("[BEACON]   2. Start:   sudo tailscaled");
                warn!("[BEACON]   3. Login:   tailscale up");
                warn!("[BEACON]   4. Verify:  tailscale status");
                warn!("[BEACON] Remote trader will need WEBSOCKET_URL set manually");
            }
        }

        // Create trading crate clients for local execution based on CONTROLLER_PLATFORMS
        let trading_kalshi: Option<Arc<trading::kalshi::KalshiApiClient>> =
            if local_platforms.contains(&TradingPlatform::Kalshi) {
                info!("[HYBRID] Creating Kalshi client for local execution");
                Some(Arc::new(trading::kalshi::KalshiApiClient::new(
                    trading::kalshi::KalshiConfig::from_env()?,
                )))
            } else {
                None
            };

        let trading_poly: Option<Arc<trading::polymarket::SharedAsyncClient>> =
            if local_platforms.contains(&TradingPlatform::Polymarket) {
                info!("[HYBRID] Creating Polymarket client for local execution");
                let client = trading::polymarket::PolymarketAsyncClient::new(
                    POLY_CLOB_HOST,
                    POLYGON_CHAIN_ID,
                    &poly_private_key,
                    &poly_funder,
                )?;
                let api_creds = client.derive_api_key(0).await?;
                let prepared = trading::polymarket::PreparedCreds::from_api_creds(&api_creds)?;
                let shared = Arc::new(trading::polymarket::SharedAsyncClient::new(
                    client,
                    prepared,
                    POLYGON_CHAIN_ID,
                ));
                // Load neg_risk cache
                if let Err(e) = shared.load_cache(&neg_risk_cache_path.to_string_lossy()) {
                    warn!("[HYBRID] Could not load neg_risk cache: {}", e);
                }
                Some(shared)
            } else {
                None
            };

        let hybrid_exec = Arc::new(HybridExecutor::new(
            state.clone(),
            circuit_breaker.clone(),
            trader_router,
            local_platforms,
            trading_kalshi,
            trading_poly,
            dry_run,
            Some(tui_log_tx.clone()),
        ));
        tokio::spawn(run_hybrid_execution_loop(exec_rx, hybrid_exec))
    } else {
        let engine = Arc::new(ExecutionEngine::new(
            kalshi_api.clone(),
            poly_async,
            state.clone(),
            circuit_breaker.clone(),
            position_channel,
            dry_run,
            clock.clone(),
        ));
        let exec_tui_state = tui_state.clone();
        let exec_log_tx = tui_log_tx.clone();
        tokio::spawn(run_execution_loop(exec_rx, engine, Some(exec_tui_state), Some(exec_log_tx)))
    };

    // Confirmation handler task: receives arbs needing confirmation, manages queue and TUI
    let confirm_exec_tx = exec_tx.clone();
    let confirm_queue_clone = confirm_queue.clone();
    let confirm_tui_state = tui_state.clone();
    let confirm_log_tx = tui_log_tx.clone();
    let _confirm_handle = if confirm_enabled {
        Some(tokio::spawn(async move {
            use chrono::Utc;

            // Initialize logger for audit trail
            let mut logger = match ConfirmationLogger::new() {
                Ok(l) => {
                    info!("[CONFIRM] Logging decisions to: {}", l.file_path().display());
                    Some(l)
                }
                Err(e) => {
                    warn!("[CONFIRM] Could not create logger: {} - decisions will not be persisted", e);
                    None
                }
            };

            // Options to hold channels until we need to launch the TUI
            // These get returned by TUI on exit for reuse
            let mut tui_update_rx_opt = Some(tui_update_rx);
            let mut tui_log_rx_opt = Some(tui_log_rx);

            // Channel to receive TUI receivers back when TUI exits
            let (tui_done_tx, mut tui_done_rx) = tokio::sync::mpsc::channel::<confirm_tui::TuiReceivers>(1);

            loop {
                tokio::select! {
                    // Receive arbs needing confirmation
                    Some((req, pair)) = confirm_rx.recv() => {
                        // Push to confirmation queue (returns true only for new entries)
                        let is_new = confirm_queue_clone.push(req, pair.clone()).await;
                        let tui_active = confirm_tui_state.read().await.active;

                        // Only log when a new arb is queued (not updates to existing)
                        if is_new {
                            let pending_count = confirm_queue_clone.len().await;
                            let msg = format!("[{}]  INFO [CONFIRM] Queued arb for {} ({} pending)",
                                chrono::Local::now().format("%H:%M:%S"), pair.description, pending_count);
                            if tui_active {
                                let _ = confirm_log_tx.try_send(msg);
                            } else {
                                println!("{}", msg);
                            }
                        }

                        // Launch TUI when not active and we have receivers available
                        if !tui_active && tui_update_rx_opt.is_some() {
                            let update_rx = tui_update_rx_opt.take().unwrap();
                            let log_rx = tui_log_rx_opt.take().expect("log_rx should exist");
                            let tui_queue = confirm_queue_clone.clone();
                            let tui_state_inner = confirm_tui_state.clone();
                            let tui_action_tx_clone = tui_action_tx.clone();
                            let done_tx = tui_done_tx.clone();

                            // Set TUI active BEFORE spawning to prevent race with heartbeat output
                            confirm_tui_state.write().await.active = true;

                            tokio::spawn(async move {
                                match confirm_tui::run_tui(
                                    tui_queue,
                                    tui_state_inner,
                                    update_rx,
                                    tui_action_tx_clone,
                                    log_rx,
                                ).await {
                                    Ok(receivers) => {
                                        // Return receivers for reuse
                                        let _ = done_tx.send(receivers).await;
                                    }
                                    Err(e) => {
                                        error!("[CONFIRM] TUI error: {}", e);
                                    }
                                }
                            });
                        }
                    }

                    // TUI exited - reclaim receivers for next launch
                    Some(receivers) = tui_done_rx.recv() => {
                        tui_update_rx_opt = Some(receivers.update_rx);
                        tui_log_rx_opt = Some(receivers.log_rx);
                    }

                    // Process user actions from TUI
                    Some(action) = tui_action_rx.recv() => {
                        // Get the front arb
                        if let Some(arb) = confirm_queue_clone.pop_front().await {
                            let market_id = arb.request.market_id;

                            // Helper to log with TUI routing
                            let tui_active = confirm_tui_state.read().await.active;
                            let log_msg = |msg: String| {
                                if tui_active {
                                    let _ = confirm_log_tx.try_send(msg);
                                } else {
                                    println!("{}", msg);
                                }
                            };

                            let status = match &action {
                                ConfirmAction::Proceed => {
                                    // Validate arb is still profitable
                                    match confirm_queue_clone.validate_arb_detailed(&arb) {
                                        Some(result) if result.is_valid => {
                                            log_msg(format!("[{}]  INFO [CONFIRM] ✅ Approved: {} - forwarding to execution",
                                                chrono::Local::now().format("%H:%M:%S"), arb.pair.description));
                                            let _ = confirm_exec_tx.try_send(arb.request.clone());
                                            ConfirmationStatus::Accepted
                                        }
                                        Some(result) => {
                                            // Expired - show how much prices moved
                                            let cost_change = result.current_cost as i32 - result.original_cost as i32;
                                            log_msg(format!(
                                                "[{}]  WARN [CONFIRM] ⚠️ Approved but EXPIRED: {} - cost {}c → {}c ({:+}c)",
                                                chrono::Local::now().format("%H:%M:%S"),
                                                arb.pair.description,
                                                result.original_cost,
                                                result.current_cost,
                                                cost_change
                                            ));
                                            ConfirmationStatus::AcceptedExpired
                                        }
                                        None => {
                                            log_msg(format!("[{}]  WARN [CONFIRM] ⚠️ Approved but EXPIRED: {} - market not found",
                                                chrono::Local::now().format("%H:%M:%S"), arb.pair.description));
                                            ConfirmationStatus::AcceptedExpired
                                        }
                                    }
                                }
                                ConfirmAction::Reject { note } => {
                                    log_msg(format!("[{}]  INFO [CONFIRM] ❌ Rejected: {}{}",
                                        chrono::Local::now().format("%H:%M:%S"), arb.pair.description,
                                        note.as_ref().map(|n| format!(" ({})", n)).unwrap_or_default()));
                                    ConfirmationStatus::Rejected
                                }
                                ConfirmAction::Blacklist { note } => {
                                    log_msg(format!("[{}]  INFO [CONFIRM] 🚫 Blacklisted: {}{}",
                                        chrono::Local::now().format("%H:%M:%S"), arb.pair.description,
                                        note.as_ref().map(|n| format!(" ({})", n)).unwrap_or_default()));
                                    confirm_queue_clone.blacklist_market(market_id).await;
                                    ConfirmationStatus::Blacklisted
                                }
                            };

                            // Log the decision
                            if let Some(ref mut log) = logger {
                                let note = match &action {
                                    ConfirmAction::Reject { note } | ConfirmAction::Blacklist { note } => note.clone(),
                                    _ => None,
                                };
                                let record = ConfirmationRecord {
                                    timestamp: Utc::now(),
                                    status,
                                    market_id,
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
                                if let Err(e) = log.log(record) {
                                    log_msg(format!("[{}]  WARN [CONFIRM] Failed to log decision: {}",
                                        chrono::Local::now().format("%H:%M:%S"), e));
                                }
                            }
                        }
                    }

                    else => break,
                }
            }
        }))
    } else {
        // When confirm mode is disabled, drain confirm_rx and forward directly to execution
        Some(tokio::spawn(async move {
            while let Some((req, _pair)) = confirm_rx.recv().await {
                let _ = confirm_exec_tx.try_send(req);
            }
        }))
    };

    // === TEST MODE: Synthetic arbitrage injection ===
    // TEST_ARB=1 to enable, TEST_ARB_TYPE=poly_yes_kalshi_no|kalshi_yes_poly_no|poly_only|kalshi_only
    let test_arb = std::env::var("TEST_ARB").map(|v| v == "1" || v == "true").unwrap_or(false);
    if test_arb {
        let test_state = state.clone();
        let test_exec_tx = exec_tx.clone();
        let test_confirm_tx = confirm_tx.clone();
        let test_dry_run = dry_run;

        // Parse arb type from environment (default: poly_yes_kalshi_no)
        let arb_type_str = std::env::var("TEST_ARB_TYPE").unwrap_or_else(|_| "poly_yes_kalshi_no".to_string());

        // Parse delay from environment (default: 10 seconds)
        let test_arb_delay: u64 = std::env::var("TEST_ARB_DELAY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        tokio::spawn(async move {
            use types::{FastExecutionRequest, ArbType};

            // Wait for WebSocket connections to establish and populate orderbooks
            info!("[TEST] Injecting synthetic arbitrage opportunity in {} seconds...", test_arb_delay);
            tokio::time::sleep(tokio::time::Duration::from_secs(test_arb_delay)).await;

            // Parse arb type
            let arb_type = match arb_type_str.to_lowercase().as_str() {
                "poly_yes_kalshi_no" | "pykn" | "0" => ArbType::PolyYesKalshiNo,
                "kalshi_yes_poly_no" | "kypn" | "1" => ArbType::KalshiYesPolyNo,
                "poly_only" | "poly" | "2" => ArbType::PolyOnly,
                "kalshi_only" | "kalshi" | "3" => ArbType::KalshiOnly,
                _ => {
                    warn!("[TEST] Unknown TEST_ARB_TYPE='{}', defaulting to PolyYesKalshiNo", arb_type_str);
                    warn!("[TEST] Valid values: poly_yes_kalshi_no, kalshi_yes_poly_no, poly_only, kalshi_only");
                    ArbType::PolyYesKalshiNo
                }
            };

            // Set prices based on arb type for realistic test scenarios
            let (yes_price, no_price, description) = match arb_type {
                ArbType::PolyYesKalshiNo => (40, 50, "P_yes=40¢ + K_no=50¢ + fee≈2¢ = 92¢ → 8¢ profit"),
                ArbType::KalshiYesPolyNo => (40, 50, "K_yes=40¢ + P_no=50¢ + fee≈2¢ = 92¢ → 8¢ profit"),
                ArbType::PolyOnly => (48, 50, "P_yes=48¢ + P_no=50¢ + fee=0¢ = 98¢ → 2¢ profit (NO FEES!)"),
                ArbType::KalshiOnly => (44, 44, "K_yes=44¢ + K_no=44¢ + fee≈4¢ = 92¢ → 8¢ profit (DOUBLE FEES)"),
            };

            // Find first market with valid state
            let market_count = test_state.market_count();
            for market_id in 0..market_count {
                if let Some(market) = test_state.get_by_id(market_id as u16) {
                    if let Some(pair) = market.pair() {
                        // SIZE: 1000 cents = 10 contracts (Poly $1 min requires ~3 contracts at 40¢)
                        let fake_req = FastExecutionRequest {
                            market_id: market_id as u16,
                            yes_price,
                            no_price,
                            yes_size: 1000,  // 1000¢ = 10 contracts
                            no_size: 1000,   // 1000¢ = 10 contracts
                            arb_type,
                            detected_ns: 0,
                            is_test: true,
                        };

                        warn!("[TEST] 🧪 Injecting synthetic {:?} arbitrage for: {}", arb_type, pair.description);
                        warn!("[TEST]    Scenario: {}", description);
                        warn!("[TEST]    Position size capped to 10 contracts for safety");
                        warn!("[TEST]    Execution mode: DRY_RUN={}", test_dry_run);

                        // Route based on confirmation requirement (same logic as WebSocket handlers)
                        if config::requires_confirmation(&pair.league) {
                            warn!("[TEST]    Routing to confirm queue (league {} requires confirmation)", pair.league);
                            if let Err(e) = test_confirm_tx.send((fake_req, pair)).await {
                                error!("[TEST] Failed to send to confirm queue: {}", e);
                            }
                        } else {
                            if let Err(e) = test_exec_tx.send(fake_req).await {
                                error!("[TEST] Failed to send fake arb: {}", e);
                            }
                        }
                        break;
                    }
                }
            }
        });
    }

    // Initialize Kalshi WebSocket connection (config reused on reconnects)
    let kalshi_state = state.clone();
    let kalshi_exec_tx = exec_tx.clone();
    let kalshi_confirm_tx = confirm_tx.clone();
    let kalshi_threshold = threshold_cents;
    let kalshi_ws_config = KalshiConfig::from_env()?;
    let kalshi_shutdown_rx = shutdown_rx.clone();
    let kalshi_clock = clock.clone();
    let kalshi_tui_state = tui_state.clone();
    let kalshi_log_tx = tui_log_tx.clone();
    let kalshi_handle = tokio::spawn(async move {
        loop {
            let shutdown_rx = kalshi_shutdown_rx.clone();
            if let Err(e) = kalshi::run_ws(
                &kalshi_ws_config,
                kalshi_state.clone(),
                kalshi_exec_tx.clone(),
                kalshi_confirm_tx.clone(),
                kalshi_threshold,
                shutdown_rx,
                kalshi_clock.clone(),
                kalshi_tui_state.clone(),
                kalshi_log_tx.clone(),
            )
            .await
            {
                let tui_active = kalshi_tui_state.read().await.active;
                if tui_active {
                    let _ = kalshi_log_tx.try_send(format!("[{}] ERROR [KALSHI] WebSocket disconnected: {} - reconnecting...",
                        chrono::Local::now().format("%H:%M:%S"), e));
                } else {
                    error!("[KALSHI] WebSocket disconnected: {} - reconnecting...", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // Initialize Polymarket WebSocket connection
    let poly_state = state.clone();
    let poly_exec_tx = exec_tx.clone();
    let poly_confirm_tx = confirm_tx.clone();
    let poly_threshold = threshold_cents;
    let poly_shutdown_rx = shutdown_rx.clone();
    let poly_clock = clock.clone();
    let poly_tui_state = tui_state.clone();
    let poly_log_tx = tui_log_tx.clone();
    let poly_handle = tokio::spawn(async move {
        loop {
            let shutdown_rx = poly_shutdown_rx.clone();
            if let Err(e) = polymarket::run_ws(
                poly_state.clone(),
                poly_exec_tx.clone(),
                poly_confirm_tx.clone(),
                poly_threshold,
                shutdown_rx,
                poly_clock.clone(),
                poly_tui_state.clone(),
                poly_log_tx.clone(),
            )
            .await
            {
                let tui_active = poly_tui_state.read().await.active;
                if tui_active {
                    let _ = poly_log_tx.try_send(format!("[{}] ERROR [POLYMARKET] WebSocket disconnected: {} - reconnecting...",
                        chrono::Local::now().format("%H:%M:%S"), e));
                } else {
                    error!("[POLYMARKET] WebSocket disconnected: {} - reconnecting...", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // Startup sweep: scan all markets for arbs after WebSockets settle
    // This catches opportunities that existed before both platforms were fully loaded
    let sweep_state = state.clone();
    let sweep_exec_tx = exec_tx.clone();
    let sweep_threshold = threshold_cents;
    let sweep_clock = clock.clone();
    let sweep_tui_state = tui_state.clone();
    let sweep_log_tx = tui_log_tx.clone();
    tokio::spawn(async move {
        use crate::types::{FastExecutionRequest, ArbType};

        // Helper closure to route log output based on TUI state
        let log_line = |line: String, tui_active: bool| {
            if tui_active {
                let _ = sweep_log_tx.try_send(line);
            } else {
                info!("{}", line);
            }
        };

        // Wait for WebSockets to connect and receive initial snapshots
        const STARTUP_SWEEP_DELAY_SECS: u64 = 10;
        let tui_active = sweep_tui_state.read().await.active;
        log_line(format!("[SWEEP] Startup sweep scheduled in {}s...", STARTUP_SWEEP_DELAY_SECS), tui_active);
        tokio::time::sleep(tokio::time::Duration::from_secs(STARTUP_SWEEP_DELAY_SECS)).await;

        let market_count = sweep_state.market_count();
        let mut arbs_found = 0;
        let mut markets_scanned = 0;

        for market in sweep_state.markets.iter().take(market_count) {
            let (k_yes, k_no, _, _) = market.kalshi.load();
            let (p_yes, p_no, _, _) = market.poly.load();

            // Only check markets with both platforms populated
            if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
                continue;
            }
            markets_scanned += 1;

            let arb_mask = market.check_arbs(sweep_threshold);
            if arb_mask != 0 {
                arbs_found += 1;

                // Build execution request (same logic as send_arb_request)
                let (k_yes, k_no, k_yes_size, k_no_size) = market.kalshi.load();
                let (p_yes, p_no, p_yes_size, p_no_size) = market.poly.load();

                let (yes_price, no_price, yes_size, no_size, arb_type) = if arb_mask & 1 != 0 {
                    (p_yes, k_no, p_yes_size, k_no_size, ArbType::PolyYesKalshiNo)
                } else if arb_mask & 2 != 0 {
                    (k_yes, p_no, k_yes_size, p_no_size, ArbType::KalshiYesPolyNo)
                } else if arb_mask & 4 != 0 {
                    (p_yes, p_no, p_yes_size, p_no_size, ArbType::PolyOnly)
                } else if arb_mask & 8 != 0 {
                    (k_yes, k_no, k_yes_size, k_no_size, ArbType::KalshiOnly)
                } else {
                    continue;
                };

                let req = FastExecutionRequest {
                    market_id: market.market_id,
                    yes_price,
                    no_price,
                    yes_size,
                    no_size,
                    arb_type,
                    detected_ns: sweep_clock.now_ns(),
                    is_test: false,
                };

                if let Err(e) = sweep_exec_tx.try_send(req) {
                    warn!("[SWEEP] Failed to send arb request: {}", e);
                }
            }
        }

        let tui_active = sweep_tui_state.read().await.active;
        log_line(format!("[SWEEP] Startup sweep complete: scanned {} markets, found {} arbs",
              markets_scanned, arbs_found), tui_active);
    });

    // System health monitoring and arbitrage diagnostics
    let heartbeat_state = state.clone();
    let heartbeat_threshold = threshold_cents;
    let heartbeat_tui_state = tui_state.clone();
    let heartbeat_log_tx = tui_log_tx.clone();
    let heartbeat_handle = tokio::spawn(async move {
        use crate::types::kalshi_fee_cents;
        use std::collections::HashMap;

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            config::heartbeat_interval_secs(),
        ));

        // Track previous update totals for delta calculation
        let mut prev_kalshi_updates: u64 = 0;
        let mut prev_poly_updates: u64 = 0;

        // Track previous stats for delta calculation
        let mut prev_league_type_stats: HashMap<(String, MarketType), (u32, u32)> = HashMap::new();
        loop {
            interval.tick().await;

            // Check TUI state once per iteration for efficient log routing
            let tui_active = heartbeat_tui_state.read().await.active;

            // Helper closure to route log output
            let log_line = |line: String| {
                if tui_active {
                    let _ = heartbeat_log_tx.try_send(line);
                } else {
                    println!("{}", line);
                }
            };

            let market_count = heartbeat_state.market_count();
            let verbose = config::verbose_heartbeat_enabled();
            let now_ms = now_unix_ms();

            // Aggregate stats by (league, market_type)
            // Value: (market_count, kalshi_updates, poly_updates)
            let mut league_type_stats: HashMap<(String, MarketType), (usize, u32, u32)> = HashMap::new();

            // For verbose mode: collect market details
            struct MarketDetail {
                description: String,
                league: String,
                market_type: MarketType,
                k_yes: u16,
                k_no: u16,
                p_yes: u16,
                p_no: u16,
                gap: i16,
                yes_size: u16,  // Size of YES leg for best arb
                no_size: u16,   // Size of NO leg for best arb
                k_updates: u32,
                p_updates: u32,
                k_last_ms: u64,
                p_last_ms: u64,
            }
            let mut market_details: Vec<MarketDetail> = Vec::new();

            // Totals for header
            let mut total_kalshi_updates: u64 = 0;
            let mut total_poly_updates: u64 = 0;
            let mut with_both = 0usize;
            // Track best arbitrage opportunity:
            // (total_cost, market_id, p_yes, k_no, k_yes, p_no, fee, is_poly_yes_kalshi_no, yes_size, no_size)
            #[allow(clippy::type_complexity)]
            let mut best_arb: Option<(u16, u16, u16, u16, u16, u16, u16, bool, u16, u16)> = None;

            for market in heartbeat_state.markets.iter().take(market_count) {
                let (k_yes, k_no, k_yes_size, k_no_size) = market.kalshi.load();
                let (p_yes, p_no, p_yes_size, p_no_size) = market.poly.load();
                let (k_upd, p_upd) = market.load_update_counts();
                let (k_last_ms, p_last_ms) = market.last_updates_unix_ms();

                total_kalshi_updates += k_upd as u64;
                total_poly_updates += p_upd as u64;

                let has_k = k_yes > 0 && k_no > 0;
                let has_p = p_yes > 0 && p_no > 0;

                // Aggregate by league/type
                if let Some(pair) = market.pair() {
                    let key = (pair.league.to_string(), pair.market_type);
                    let entry = league_type_stats.entry(key).or_insert((0, 0, 0));
                    entry.0 += 1;
                    entry.1 += k_upd;
                    entry.2 += p_upd;

                    // For verbose mode, collect ALL discovered markets (not just those with both prices)
                    if verbose {
                        // Calculate gap and sizes only if both platforms have prices
                        let (gap, yes_size, no_size) = if has_k && has_p {
                            let fee1 = kalshi_fee_cents(k_no);
                            let cost1 = p_yes + k_no + fee1;
                            let fee2 = kalshi_fee_cents(k_yes);
                            let cost2 = k_yes + fee2 + p_no;
                            // Determine which arb type is better and use those sizes
                            if cost1 <= cost2 {
                                // PolyYesKalshiNo: YES from Poly, NO from Kalshi
                                (cost1 as i16 - heartbeat_threshold as i16, p_yes_size, k_no_size)
                            } else {
                                // KalshiYesPolyNo: YES from Kalshi, NO from Poly
                                (cost2 as i16 - heartbeat_threshold as i16, k_yes_size, p_no_size)
                            }
                        } else {
                            (i16::MAX, 0, 0) // Sentinel value indicating no gap calculable
                        };

                        market_details.push(MarketDetail {
                            description: pair.description.to_string(),
                            league: pair.league.to_string(),
                            market_type: pair.market_type,
                            k_yes,
                            k_no,
                            p_yes,
                            p_no,
                            gap,
                            yes_size,
                            no_size,
                            k_updates: k_upd,
                            p_updates: p_upd,
                            k_last_ms,
                            p_last_ms,
                        });
                    }
                }

                if has_k && has_p {
                    with_both += 1;

                    let fee1 = kalshi_fee_cents(k_no);
                    let cost1 = p_yes + k_no + fee1;

                    let fee2 = kalshi_fee_cents(k_yes);
                    let cost2 = k_yes + fee2 + p_no;

                    let (best_cost, best_fee, is_poly_yes, yes_size, no_size) = if cost1 <= cost2 {
                        (cost1, fee1, true, p_yes_size, k_no_size)
                    } else {
                        (cost2, fee2, false, k_yes_size, p_no_size)
                    };

                    if best_arb.is_none() || best_cost < best_arb.as_ref().unwrap().0 {
                        best_arb = Some((
                            best_cost,
                            market.market_id,
                            p_yes,
                            k_no,
                            k_yes,
                            p_no,
                            best_fee,
                            is_poly_yes,
                            yes_size,
                            no_size,
                        ));
                    }
                }
            }

            // Calculate update deltas since last heartbeat
            let kalshi_delta = total_kalshi_updates.saturating_sub(prev_kalshi_updates);
            let poly_delta = total_poly_updates.saturating_sub(prev_poly_updates);
            prev_kalshi_updates = total_kalshi_updates;
            prev_poly_updates = total_poly_updates;

            if verbose {
                // Verbose mode: hierarchical tree view
                log_line(String::new());
                log_line(format!("[{}]  INFO controller: 💓 VERBOSE - {} markets", chrono::Local::now().format("%H:%M:%S"), market_count));
                log_line(String::new());

                // Group by league, then by market type
                let mut by_league: HashMap<String, Vec<&MarketDetail>> = HashMap::new();
                for detail in &market_details {
                    by_league.entry(detail.league.clone()).or_default().push(detail);
                }

                let mut sorted_leagues: Vec<_> = by_league.keys().cloned().collect();
                sorted_leagues.sort();

                for league in sorted_leagues {
                    let markets = by_league.get(&league).unwrap();
                    let league_updates: u64 = markets.iter().map(|m| m.k_updates as u64 + m.p_updates as u64).sum();

                    log_line(format!("📊 {} ({} markets, {} updates)",
                             league.to_uppercase(), markets.len(), league_updates));

                    // Group by market type
                    let mut by_type: HashMap<MarketType, Vec<&&MarketDetail>> = HashMap::new();
                    for m in markets {
                        by_type.entry(m.market_type).or_default().push(m);
                    }

                    let type_order = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
                    let type_count = by_type.len();
                    let mut type_idx = 0;

                    for mt in type_order.iter() {
                        if let Some(type_markets) = by_type.get_mut(mt) {
                            // Sort markets by description to group by event
                            type_markets.sort_by(|a, b| a.description.cmp(&b.description));

                            type_idx += 1;
                            let is_last_type = type_idx == type_count;
                            let branch = if is_last_type { "└" } else { "├" };

                            log_line(format!("{}─ {:?} ({})", branch, mt, type_markets.len()));

                            // Show all markets
                            for (i, m) in type_markets.iter().enumerate() {
                                let is_last = i == type_markets.len() - 1;
                                let prefix = if is_last_type { " " } else { "│" };
                                let item_branch = if is_last { "└" } else { "├" };

                                // Strip redundant market type, deduplicate, and truncate to 55 chars
                                // Use char-aware truncation to avoid panic on UTF-8 boundaries
                                let stripped = strip_market_type_suffix(&m.description, mt);
                                let deduped = deduplicate_market_name(&stripped);
                                let desc: String = if deduped.chars().count() > 55 {
                                    let truncated: String = deduped.chars().take(52).collect();
                                    format!("{}...", truncated)
                                } else {
                                    deduped
                                };

                                // Format Kalshi prices (-- if no prices)
                                let k_str = if m.k_yes > 0 && m.k_no > 0 {
                                    format!("{:02}/{:02}", m.k_yes, m.k_no)
                                } else {
                                    "--/--".to_string()
                                };

                                // Format Polymarket prices (-- if no prices)
                                let p_str = if m.p_yes > 0 && m.p_no > 0 {
                                    format!("{:02}/{:02}", m.p_yes, m.p_no)
                                } else {
                                    "--/--".to_string()
                                };

                                // Format gap (-- if not calculable)
                                let gap_str = if m.gap == i16::MAX {
                                    "\x1b[33m--\x1b[0m".to_string() // Yellow for missing
                                } else if m.gap < 0 {
                                    format!("\x1b[32m{:+}¢\x1b[0m", m.gap) // Green for arb
                                } else {
                                    format!("\x1b[31m{:+}¢\x1b[0m", m.gap) // Red for no arb
                                };

                                // Format size in dollars (cents / 100)
                                let size_str = if m.yes_size > 0 || m.no_size > 0 {
                                    let yes_dollars = m.yes_size as f64 / 100.0;
                                    let no_dollars = m.no_size as f64 / 100.0;
                                    format!("${:.0}/${:.0}", yes_dollars, no_dollars)
                                } else {
                                    "--/--".to_string()
                                };

                                let k_time = fmt_unix_ms_hhmmss(m.k_last_ms);
                                let p_time = fmt_unix_ms_hhmmss(m.p_last_ms);
                                let k_age = fmt_age(now_ms, m.k_last_ms);
                                let p_age = fmt_age(now_ms, m.p_last_ms);

                                log_line(format!(
                                    "{}  {}── {:<55} K:{} P:{} gap:{} size:{}    upd:K{}/P{} last:K{}({}) P{}({})",
                                         prefix, item_branch, desc,
                                         k_str, p_str,
                                         gap_str, size_str, m.k_updates, m.p_updates,
                                         k_time, k_age, p_time, p_age
                                ));
                            }
                        }
                    }
                    log_line(String::new());
                }

                log_line(format!("Legend: gap = cost - {}¢ | negative = arb opportunity", heartbeat_threshold));
                log_line(String::new());
            } else {
                // Default mode: compact summary
                log_line(String::new());
                log_line(format!("[{}]  INFO controller: 💓 {} markets | K:{} P:{} updates/min",
                      chrono::Local::now().format("%H:%M:%S"), market_count, kalshi_delta, poly_delta));
            }

            // Log best opportunity
            if let Some((cost, market_id, p_yes, k_no, k_yes, p_no, fee, is_poly_yes, yes_size, no_size)) = best_arb {
                let gap = cost as i16 - heartbeat_threshold as i16;
                let pair = heartbeat_state.get_by_id(market_id)
                    .and_then(|m| m.pair());
                let desc = pair
                    .as_ref()
                    .map(|p| {
                        if let Some(line) = p.line_value {
                            format!("{} ({})", p.description, line)
                        } else {
                            p.description.to_string()
                        }
                    })
                    .unwrap_or_else(|| "Unknown".to_string());
                let leg_breakdown = if is_poly_yes {
                    format!("P_yes({}¢) + K_no({}¢) + K_fee({}¢) = {}¢", p_yes, k_no, fee, cost)
                } else {
                    format!("K_yes({}¢) + P_no({}¢) + K_fee({}¢) = {}¢", k_yes, p_no, fee, cost)
                };
                if gap < 0 {
                    log_line(String::new());
                    log_line(format!(
                        "[{}]  INFO controller: 📊 Best opportunity: {} | {} | gap={:+}¢ | size={}¢/{}¢ | [Poly_yes={}¢ Kalshi_no={}¢ Kalshi_yes={}¢ Poly_no={}¢]",
                        chrono::Local::now().format("%H:%M:%S"),
                        desc,
                        leg_breakdown,
                        gap,
                        yes_size,
                        no_size,
                        p_yes,
                        k_no,
                        k_yes,
                        p_no
                    ));
                    // Log URLs for easy access
                    if let Some(p) = pair.as_ref() {
                        let kalshi_series = p.kalshi_event_ticker
                            .split('-')
                            .next()
                            .unwrap_or(&p.kalshi_event_ticker)
                            .to_lowercase();
                        let kalshi_event_ticker_lower = p.kalshi_event_ticker.to_lowercase();
                        let poly_url = config::build_polymarket_url(&p.league, &p.poly_slug);
                        let kalshi_url = format!("{}/{}/{}/{}", config::KALSHI_WEB_BASE, kalshi_series, p.kalshi_event_slug, kalshi_event_ticker_lower);
                        log_line(format!("[{}]  INFO controller: 🔗 Kalshi: {} | Polymarket: {}",
                              chrono::Local::now().format("%H:%M:%S"),
                              kalshi_url,
                              poly_url));
                    }
                }
            } else if with_both == 0 {
                log_line(format!("[{}]  WARN controller: ⚠️  No markets with both Kalshi and Polymarket prices - verify WebSocket connections",
                    chrono::Local::now().format("%H:%M:%S")));
            }

            // Print league summary table
            let market_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
            let type_headers = ["Moneyline", "Spread", "Total", "BTTS"];

            let mut leagues: Vec<String> = league_type_stats.keys()
                .map(|(l, _)| l.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            leagues.sort();

            if !leagues.is_empty() {
                // Print header
                log_line("┌──────────┬────────────┬────────────┬────────────┬────────────┐".to_string());
                log_line(format!("│ {:8} │ {:^10} │ {:^10} │ {:^10} │ {:^10} │",
                         "League", type_headers[0], type_headers[1], type_headers[2], type_headers[3]));
                log_line("├──────────┼────────────┼────────────┼────────────┼────────────┤".to_string());

                for league in &leagues {
                    let mut cells: Vec<String> = Vec::new();
                    for mt in &market_types {
                        if let Some(&(count, k_upd, p_upd)) = league_type_stats.get(&(league.clone(), *mt)) {
                            let key = (league.clone(), *mt);
                            let (prev_k, prev_p) = prev_league_type_stats.get(&key).copied().unwrap_or((0, 0));
                            let delta = (k_upd.saturating_sub(prev_k)) + (p_upd.saturating_sub(prev_p));
                            cells.push(format!("{} (+{})", count, delta));
                        } else {
                            cells.push("-".to_string());
                        }
                    }
                    log_line(format!("│ {:8} │ {:^10} │ {:^10} │ {:^10} │ {:^10} │",
                             league, cells[0], cells[1], cells[2], cells[3]));
                }

                log_line("└──────────┴────────────┴────────────┴────────────┴────────────┘".to_string());

                // Update previous stats for next iteration
                for (key, &(_, k_upd, p_upd)) in &league_type_stats {
                    prev_league_type_stats.insert(key.clone(), (k_upd, p_upd));
                }
            }
        }
    });

    // Spawn discovery refresh task
    let discovery_interval = config::discovery_interval_mins();
    let discovery_client = Arc::new(discovery);
    let discovery_state = state.clone();
    let refresh_leagues = leagues_owned.clone();
    let discovery_tui_state = tui_state.clone();
    let discovery_log_tx = tui_log_tx.clone();
    let discovery_handle = tokio::spawn(async move {
        discovery_refresh_task(
            discovery_client,
            discovery_state,
            shutdown_tx,
            discovery_interval,
            refresh_leagues,
            discovery_tui_state,
            discovery_log_tx,
        ).await;
    });

    // Main event loop - run until termination
    info!("✅ All systems operational - entering main event loop");
    let _ = tokio::join!(kalshi_handle, poly_handle, heartbeat_handle, exec_handle, discovery_handle);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deduplicate_market_name_with_duplicate_prefix() {
        // Full duplication with additional text
        assert_eq!(
            deduplicate_market_name("Union Berlin vs Dortmund - Union Berlin vs Dortmund Winner?"),
            "Union Berlin vs Dortmund Winner?"
        );
    }

    #[test]
    fn test_deduplicate_market_name_exact_duplicate() {
        // Exact duplicate on both sides
        assert_eq!(
            deduplicate_market_name("Dortmund at Union Berlin: Totals - Dortmund at Union Berlin: Totals"),
            "Dortmund at Union Berlin: Totals"
        );
    }

    #[test]
    fn test_deduplicate_market_name_no_separator() {
        // No separator - should return unchanged
        assert_eq!(
            deduplicate_market_name("Simple Market Name"),
            "Simple Market Name"
        );
    }

    #[test]
    fn test_deduplicate_market_name_no_duplicate() {
        // Has separator but suffix doesn't start with prefix
        assert_eq!(
            deduplicate_market_name("Team A vs Team B - Winner takes all"),
            "Team A vs Team B - Winner takes all"
        );
    }

    #[test]
    fn test_deduplicate_market_name_partial_match() {
        // Prefix is partial match but not exact start
        assert_eq!(
            deduplicate_market_name("Lakers vs Celtics - Lakers win by 10+"),
            "Lakers vs Celtics - Lakers win by 10+"
        );
    }

    #[test]
    fn test_deduplicate_market_name_spread_example() {
        // Real spread market example
        assert_eq!(
            deduplicate_market_name("Lakers at Celtics: Spread - Lakers at Celtics: Spread +5.5"),
            "Lakers at Celtics: Spread +5.5"
        );
    }

    #[test]
    fn test_deduplicate_market_name_empty() {
        assert_eq!(deduplicate_market_name(""), "");
    }

    #[test]
    fn test_deduplicate_market_name_only_separator() {
        assert_eq!(deduplicate_market_name(" - "), "");
    }

    // Tests for strip_market_type_suffix

    #[test]
    fn test_strip_spread_suffix() {
        assert_eq!(
            strip_market_type_suffix(
                "Leeds at Everton: Spreads - Everton wins by over 1.5 goals?",
                &types::MarketType::Spread
            ),
            "Leeds at Everton - Everton by 1.5"
        );
    }

    #[test]
    fn test_strip_spread_suffix_short() {
        assert_eq!(
            strip_market_type_suffix(
                "Leeds at Everton - Manchester City wins by over 2.5 goals?",
                &types::MarketType::Spread
            ),
            "Leeds at Everton - Manchester City by 2.5"
        );
    }

    #[test]
    fn test_strip_totals_suffix() {
        assert_eq!(
            strip_market_type_suffix(
                "Leeds at Everton: Totals 2.5",
                &types::MarketType::Total
            ),
            "Leeds at Everton 2.5"
        );
    }

    #[test]
    fn test_strip_totals_suffix_no_value() {
        assert_eq!(
            strip_market_type_suffix(
                "Leeds at Everton: Totals",
                &types::MarketType::Total
            ),
            "Leeds at Everton"
        );
    }

    #[test]
    fn test_strip_btts_suffix() {
        assert_eq!(
            strip_market_type_suffix(
                "Leeds at Everton: Both Teams to Score",
                &types::MarketType::Btts
            ),
            "Leeds at Everton"
        );
    }

    #[test]
    fn test_strip_moneyline_unchanged() {
        assert_eq!(
            strip_market_type_suffix(
                "Leeds at Everton - Everton wins",
                &types::MarketType::Moneyline
            ),
            "Leeds at Everton - Everton wins"
        );
    }

    #[test]
    fn test_strip_moneyline_winner() {
        assert_eq!(
            strip_market_type_suffix(
                "Union Berlin vs Dortmund Winner?",
                &types::MarketType::Moneyline
            ),
            "Union Berlin vs Dortmund"
        );
    }

    #[test]
    fn test_strip_moneyline_winner_no_question() {
        assert_eq!(
            strip_market_type_suffix(
                "Union Berlin vs Dortmund Winner",
                &types::MarketType::Moneyline
            ),
            "Union Berlin vs Dortmund"
        );
    }

    #[test]
    fn test_strip_moneyline_tie_to_draw() {
        assert_eq!(
            strip_market_type_suffix(
                "Union Berlin vs Dortmund (Tie)",
                &types::MarketType::Moneyline
            ),
            "Union Berlin vs Dortmund (Draw)"
        );
    }
}
