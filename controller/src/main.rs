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
mod discovery;
mod execution;
mod kalshi;
mod paths;
mod polymarket;
mod polymarket_clob;
mod position_tracker;
mod remote_execution;
mod remote_protocol;
mod remote_trader;
mod types;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tailscale::beacon::BeaconSender;
use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Global counter for lines printed since last table clear.
static LINES_SINCE_TABLE: AtomicUsize = AtomicUsize::new(0);

/// Custom tracing layer that counts output lines for table replacement.
/// Estimates visual lines based on message length and terminal width.
struct LineCountingLayer;

impl<S> tracing_subscriber::Layer<S> for LineCountingLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        // Get actual terminal width, fallback to 80
        let term_width = terminal_size::terminal_size()
            .map(|(w, _)| w.0 as usize)
            .unwrap_or(80);

        // Extract message length from event
        struct MsgLen(usize);
        impl tracing::field::Visit for MsgLen {
            fn record_debug(&mut self, _field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0 += format!("{:?}", value).len();
            }
            fn record_str(&mut self, _field: &tracing::field::Field, value: &str) {
                self.0 += value.len();
            }
        }

        let mut visitor = MsgLen(60); // Base overhead for timestamp, level, target, emojis
        event.record(&mut visitor);

        // Calculate visual lines - be conservative (emojis take 2 chars, add buffer)
        // Use 1.5x message length estimate and always at least 1 line
        let visual_lines = ((visitor.0 * 3 / 2) / term_width).max(1) + 1;
        LINES_SINCE_TABLE.fetch_add(visual_lines, Ordering::Relaxed);
    }
}

use cache::TeamCache;
use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use config::{ARB_THRESHOLD, enabled_leagues, get_league_configs, parse_controller_platforms, WS_RECONNECT_DELAY_SECS};
use discovery::DiscoveryClient;
use execution::{ExecutionEngine, NanoClock, create_execution_channel, run_execution_loop};
use kalshi::{KalshiConfig, KalshiApiClient};
use polymarket_clob::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};
use position_tracker::{PositionTracker, create_position_channel, position_writer_loop};
use crate::remote_execution::{HybridExecutor, run_hybrid_execution_loop};
use crate::remote_protocol::Platform as WsPlatform;
use crate::remote_trader::RemoteTraderServer;
use trading::execution::Platform as TradingPlatform;
use types::{GlobalState, MarketType, PriceCents};

fn cli_has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn cli_get_value(args: &[String], flag: &str) -> Option<String> {
    for arg in args {
        if let Some(val) = arg.strip_prefix(&format!("{}=", flag)) {
            return Some(val.to_string());
        }
    }
    None
}

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

/// Background task that periodically discovers new markets
async fn discovery_refresh_task(
    discovery: Arc<DiscoveryClient>,
    state: Arc<GlobalState>,
    shutdown_tx: watch::Sender<bool>,
    interval_mins: u64,
    leagues: Vec<String>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    if interval_mins == 0 {
        info!("[DISCOVERY] Runtime discovery disabled (DISCOVERY_INTERVAL_MINS=0)");
        return;
    }

    info!("[DISCOVERY] Runtime discovery enabled (interval: {}m)", interval_mins);

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_mins * 60));
    interval.tick().await; // Skip immediate first tick

    // Track last discovery timestamp
    let mut last_discovery_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    loop {
        interval.tick().await;

        info!("[DISCOVERY] Running scheduled discovery...");

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
        for err in &result.errors {
            warn!("[DISCOVERY] {}", err);
        }

        if result.pairs.is_empty() {
            info!("[DISCOVERY] No new markets found");
            continue;
        }

        // Log new markets with highlighted formatting
        info!("[DISCOVERY] NEW MARKETS DISCOVERED: {}", result.pairs.len());
        for pair in &result.pairs {
            info!("[DISCOVERY]   -> {} | {} | {}",
                pair.league, pair.description, pair.kalshi_market_ticker);
        }

        // Add new pairs to global state (thread-safe via interior mutability)
        let mut added_count = 0;
        for pair in result.pairs {
            if let Some(market_id) = state.add_pair(pair) {
                added_count += 1;
                info!("[DISCOVERY] Added market_id {} to state", market_id);
            } else {
                warn!("[DISCOVERY] Failed to add pair - state full (MAX_MARKETS reached)");
            }
        }
        info!("[DISCOVERY] Added {} new markets to state (total: {})", added_count, state.market_count());

        // Signal WebSockets to reconnect with updated subscriptions
        info!("[DISCOVERY] Signaling WebSocket reconnect for {} new markets...", added_count);
        if shutdown_tx.send(true).is_err() {
            warn!("[DISCOVERY] Failed to signal WebSocket reconnect - receivers dropped");
        }

        // Give WebSockets time to shut down gracefully
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Reset shutdown signal for next cycle
        if shutdown_tx.send(false).is_err() {
            warn!("[DISCOVERY] Failed to reset shutdown signal - receivers dropped");
        }
    }
}

fn parse_bool_env(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v.to_lowercase() == "true" || v.to_lowercase() == "yes")
        .unwrap_or(false)
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

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging with compact local time format [HH:MM:SS]
    // Note: the binary crate is `controller`, while the shared library crate is `arb_bot`.
    // If the user doesn't set `RUST_LOG`, we still want `info` logs from both crates.
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    tracing_subscriber::registry()
        .with(LineCountingLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new("[%H:%M:%S]".to_string()))
        )
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info")
                    .add_directive("controller=info".parse().unwrap())
                    .add_directive("arb_bot=info".parse().unwrap())
            }),
        )
        .init();

    // Load environment variables from `.env` (supports workspace-root `.env`)
    paths::load_dotenv();

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

    // --- CLI overrides ---
    let args: Vec<String> = std::env::args().skip(1).collect();
    if cli_has_flag(&args, "--verbose-heartbeat") {
        std::env::set_var("VERBOSE_HEARTBEAT", "1");
    }
    if let Some(val) = cli_get_value(&args, "--heartbeat-interval") {
        std::env::set_var("HEARTBEAT_INTERVAL_SECS", val);
    }

    // Build league list for discovery from config env.
    // - If ENABLED_LEAGUES is empty, we monitor all supported leagues.
    // - Otherwise, we monitor only the explicit set.
    let leagues: Vec<&str> = {
        let enabled = enabled_leagues();
        if enabled.is_empty() {
            get_league_configs().into_iter().map(|c| c.league_code).collect()
        } else {
            enabled.iter().map(|s| s.as_str()).collect()
        }
    };
    info!("   Monitored leagues: {:?}", leagues);

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
        discovery.discover_all_force(&leagues).await
    } else {
        discovery.discover_all(&leagues).await
    };

    info!("📊 Market discovery complete:");
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
        tokio::spawn(run_execution_loop(exec_rx, engine))
    };

    // === TEST MODE: Synthetic arbitrage injection ===
    // TEST_ARB=1 to enable, TEST_ARB_TYPE=poly_yes_kalshi_no|kalshi_yes_poly_no|poly_only|kalshi_only
    let test_arb = std::env::var("TEST_ARB").map(|v| v == "1" || v == "true").unwrap_or(false);
    if test_arb {
        let test_state = state.clone();
        let test_exec_tx = exec_tx.clone();
        let test_dry_run = dry_run;

        // Parse arb type from environment (default: poly_yes_kalshi_no)
        let arb_type_str = std::env::var("TEST_ARB_TYPE").unwrap_or_else(|_| "poly_yes_kalshi_no".to_string());

        tokio::spawn(async move {
            use types::{FastExecutionRequest, ArbType};

            // Wait for WebSocket connections to establish and populate orderbooks
            info!("[TEST] Injecting synthetic arbitrage opportunity in 10 seconds...");
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

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
                        };

                        warn!("[TEST] 🧪 Injecting synthetic {:?} arbitrage for: {}", arb_type, pair.description);
                        warn!("[TEST]    Scenario: {}", description);
                        warn!("[TEST]    Position size capped to 10 contracts for safety");
                        warn!("[TEST]    Execution mode: DRY_RUN={}", test_dry_run);

                        if let Err(e) = test_exec_tx.send(fake_req).await {
                            error!("[TEST] Failed to send fake arb: {}", e);
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
    let kalshi_threshold = threshold_cents;
    let kalshi_ws_config = KalshiConfig::from_env()?;
    let kalshi_shutdown_rx = shutdown_rx.clone();
    let kalshi_clock = clock.clone();
    let kalshi_handle = tokio::spawn(async move {
        loop {
            let shutdown_rx = kalshi_shutdown_rx.clone();
            if let Err(e) = kalshi::run_ws(
                &kalshi_ws_config,
                kalshi_state.clone(),
                kalshi_exec_tx.clone(),
                kalshi_threshold,
                shutdown_rx,
                kalshi_clock.clone(),
            )
            .await
            {
                error!("[KALSHI] WebSocket disconnected: {} - reconnecting...", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // Initialize Polymarket WebSocket connection
    let poly_state = state.clone();
    let poly_exec_tx = exec_tx.clone();
    let poly_threshold = threshold_cents;
    let poly_shutdown_rx = shutdown_rx.clone();
    let poly_clock = clock.clone();
    let poly_handle = tokio::spawn(async move {
        loop {
            let shutdown_rx = poly_shutdown_rx.clone();
            if let Err(e) = polymarket::run_ws(
                poly_state.clone(),
                poly_exec_tx.clone(),
                poly_threshold,
                shutdown_rx,
                poly_clock.clone(),
            )
            .await
            {
                error!("[POLYMARKET] WebSocket disconnected: {} - reconnecting...", e);
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
    tokio::spawn(async move {
        use crate::types::{FastExecutionRequest, ArbType};

        // Wait for WebSockets to connect and receive initial snapshots
        const STARTUP_SWEEP_DELAY_SECS: u64 = 10;
        info!("[SWEEP] Startup sweep scheduled in {}s...", STARTUP_SWEEP_DELAY_SECS);
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
                };

                if let Err(e) = sweep_exec_tx.try_send(req) {
                    warn!("[SWEEP] Failed to send arb request: {}", e);
                }
            }
        }

        info!("[SWEEP] ✅ Startup sweep complete: scanned {} markets, found {} arbs",
              markets_scanned, arbs_found);
    });

    // System health monitoring and arbitrage diagnostics
    let heartbeat_state = state.clone();
    let heartbeat_threshold = threshold_cents;
    let heartbeat_handle = tokio::spawn(async move {
        use crate::types::kalshi_fee_cents;
        use std::collections::HashMap;

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            config::heartbeat_interval_secs(),
        ));

        // Track previous update totals for delta calculation
        let mut prev_kalshi_updates: u64 = 0;
        let mut prev_poly_updates: u64 = 0;

        // Clear line counter from startup logs before first heartbeat
        LINES_SINCE_TABLE.store(0, Ordering::SeqCst);

        // Track previous stats for delta calculation
        let mut prev_league_type_stats: HashMap<(String, MarketType), (u32, u32)> = HashMap::new();

        loop {
            interval.tick().await;
            let market_count = heartbeat_state.market_count();
            let verbose = config::verbose_heartbeat_enabled();

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
                k_updates: u32,
                p_updates: u32,
            }
            let mut market_details: Vec<MarketDetail> = Vec::new();

            // Totals for header
            let mut total_kalshi_updates: u64 = 0;
            let mut total_poly_updates: u64 = 0;
            let mut with_both = 0usize;

            // Track best arbitrage opportunity
            #[allow(clippy::type_complexity)]
            let mut best_arb: Option<(u16, u16, u16, u16, u16, u16, u16, bool)> = None;

            for market in heartbeat_state.markets.iter().take(market_count) {
                let (k_yes, k_no, _k_yes_size, _k_no_size) = market.kalshi.load();
                let (p_yes, p_no, _p_yes_size, _p_no_size) = market.poly.load();
                let (k_upd, p_upd) = market.load_update_counts();

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

                    // For verbose mode, collect market details
                    if verbose && has_k && has_p {
                        // Calculate gap (best cross-platform cost - threshold)
                        let fee1 = kalshi_fee_cents(k_no);
                        let cost1 = p_yes + k_no + fee1;
                        let fee2 = kalshi_fee_cents(k_yes);
                        let cost2 = k_yes + fee2 + p_no;
                        let best_cost = cost1.min(cost2);
                        let gap = best_cost as i16 - heartbeat_threshold as i16;

                        market_details.push(MarketDetail {
                            description: pair.description.to_string(),
                            league: pair.league.to_string(),
                            market_type: pair.market_type,
                            k_yes,
                            k_no,
                            p_yes,
                            p_no,
                            gap,
                            k_updates: k_upd,
                            p_updates: p_upd,
                        });
                    }
                }

                if has_k && has_p {
                    with_both += 1;

                    let fee1 = kalshi_fee_cents(k_no);
                    let cost1 = p_yes + k_no + fee1;

                    let fee2 = kalshi_fee_cents(k_yes);
                    let cost2 = k_yes + fee2 + p_no;

                    let (best_cost, best_fee, is_poly_yes) = if cost1 <= cost2 {
                        (cost1, fee1, true)
                    } else {
                        (cost2, fee2, false)
                    };

                    if best_arb.is_none() || best_cost < best_arb.as_ref().unwrap().0 {
                        best_arb = Some((best_cost, market.market_id, p_yes, k_no, k_yes, p_no, best_fee, is_poly_yes));
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
                println!();
                info!("💓 VERBOSE - {} markets", market_count);
                println!();

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

                    println!("📊 {} ({} markets, {} updates)",
                             league.to_uppercase(), markets.len(), league_updates);

                    // Group by market type
                    let mut by_type: HashMap<MarketType, Vec<&&MarketDetail>> = HashMap::new();
                    for m in markets {
                        by_type.entry(m.market_type).or_default().push(m);
                    }

                    let type_order = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
                    let type_count = by_type.len();
                    let mut type_idx = 0;

                    for mt in type_order.iter() {
                        if let Some(type_markets) = by_type.get(mt) {
                            type_idx += 1;
                            let is_last_type = type_idx == type_count;
                            let branch = if is_last_type { "└" } else { "├" };

                            println!("{}─ {:?} ({})", branch, mt, type_markets.len());

                            // Show all markets
                            for (i, m) in type_markets.iter().enumerate() {
                                let is_last = i == type_markets.len() - 1;
                                let prefix = if is_last_type { " " } else { "│" };
                                let item_branch = if is_last { "└" } else { "├" };

                                // Truncate description to 30 chars
                                let desc: String = if m.description.len() > 30 {
                                    format!("{}...", &m.description[..27])
                                } else {
                                    m.description.clone()
                                };

                                let gap_str = if m.gap < 0 {
                                    format!("\x1b[32m{:+}¢\x1b[0m", m.gap) // Green for arb
                                } else {
                                    format!("{:+}¢", m.gap)
                                };

                                println!("{}  {}── {:30} K:{:02}/{:02} P:{:02}/{:02} gap:{} upd:K{}/P{}",
                                         prefix, item_branch, desc,
                                         m.k_yes, m.k_no, m.p_yes, m.p_no,
                                         gap_str, m.k_updates, m.p_updates);
                            }
                        }
                    }
                    println!();
                }

                println!("Legend: gap = cost - {}¢ | negative = arb opportunity", heartbeat_threshold);
                println!();
            } else {
                // Default mode: compact summary
                println!();
                info!("💓 {} markets | K:{} P:{} updates/min",
                      market_count, kalshi_delta, poly_delta);
            }

            // Log best opportunity
            if let Some((cost, market_id, p_yes, k_no, k_yes, p_no, fee, is_poly_yes)) = best_arb {
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
                    info!(
                        "📊 Best opportunity: {} | {} | gap={:+}¢ | [Poly_yes={}¢ Kalshi_no={}¢ Kalshi_yes={}¢ Poly_no={}¢]",
                        desc,
                        leg_breakdown,
                        gap,
                        p_yes,
                        k_no,
                        k_yes,
                        p_no
                    );
                    // Log URLs for easy access
                    if let Some(p) = pair.as_ref() {
                        let kalshi_series = p.kalshi_event_ticker
                            .split('-')
                            .next()
                            .unwrap_or(&p.kalshi_event_ticker)
                            .to_lowercase();
                        let kalshi_event_ticker_lower = p.kalshi_event_ticker.to_lowercase();
                        let poly_url = config::build_polymarket_url(&p.league, &p.poly_slug);
                        info!("   🔗 Kalshi: {}/{}/{}/{} | Polymarket: {}",
                              config::KALSHI_WEB_BASE,
                              kalshi_series,
                              p.kalshi_event_slug,
                              kalshi_event_ticker_lower,
                              poly_url);
                    }
                }
            } else if with_both == 0 {
                warn!("⚠️  No markets with both Kalshi and Polymarket prices - verify WebSocket connections");
            }

            // Print league summary table (replaces previous table using ANSI escape codes)
            let market_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
            let type_headers = ["Moneyline", "Spread", "Total", "BTTS"];

            let mut leagues: Vec<String> = league_type_stats.keys()
                .map(|(l, _)| l.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            leagues.sort();

            if !leagues.is_empty() {
                static PREV_TABLE_LINES: AtomicUsize = AtomicUsize::new(0);

                // Get lines logged since last table and reset counter
                let extra_lines = LINES_SINCE_TABLE.swap(0, Ordering::SeqCst);
                let prev_lines = PREV_TABLE_LINES.load(Ordering::Relaxed);

                // Only replace table if few/no interleaved logs (preserve important logs)
                // If many logs appeared, just print fresh table below them
                if prev_lines > 0 && extra_lines <= 3 {
                    use std::io::Write;
                    // Move up to start of previous table, clear to end of screen
                    print!("\x1b[{}A\x1b[J", prev_lines);
                    std::io::stdout().flush().ok();
                }

                // Print header
                println!("┌──────────┬────────────┬────────────┬────────────┬────────────┐");
                println!("│ {:8} │ {:^10} │ {:^10} │ {:^10} │ {:^10} │",
                         "League", type_headers[0], type_headers[1], type_headers[2], type_headers[3]);
                println!("├──────────┼────────────┼────────────┼────────────┼────────────┤");

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
                    println!("│ {:8} │ {:^10} │ {:^10} │ {:^10} │ {:^10} │",
                             league, cells[0], cells[1], cells[2], cells[3]);
                }

                println!("└──────────┴────────────┴────────────┴────────────┴────────────┘");

                // Update previous stats for next iteration
                for (key, &(_, k_upd, p_upd)) in &league_type_stats {
                    prev_league_type_stats.insert(key.clone(), (k_upd, p_upd));
                }

                // Store table height: 3 header lines + league rows + 1 footer
                PREV_TABLE_LINES.store(4 + leagues.len(), Ordering::Relaxed);
            }
        }
    });

    // Spawn discovery refresh task
    let discovery_interval = config::discovery_interval_mins();
    let discovery_client = Arc::new(discovery);
    let discovery_state = state.clone();
    let discovery_handle = tokio::spawn(async move {
        discovery_refresh_task(
            discovery_client,
            discovery_state,
            shutdown_tx,
            discovery_interval,
            enabled_leagues().to_vec(),
        ).await;
    });

    // Main event loop - run until termination
    info!("✅ All systems operational - entering main event loop");
    let _ = tokio::join!(kalshi_handle, poly_handle, heartbeat_handle, exec_handle, discovery_handle);

    Ok(())
}
