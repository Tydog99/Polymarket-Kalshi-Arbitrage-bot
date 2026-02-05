//! Polymarket platform integration client.
//!
//! This module provides WebSocket client for real-time Polymarket price feeds
//! and REST API client for market discovery via the Gamma API.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::config::{self, POLYMARKET_WS_URL, POLY_PING_INTERVAL_SECS, GAMMA_API_BASE, POLY_MAX_TOKENS_PER_WS};
use crate::execution::NanoClock;
use crate::types::{
    GlobalState, ArbOpportunity, MarketPair, PriceCents, SizeCents,
    parse_price, fxhash_str,
};
use crate::debug_socket::{DebugBroadcaster, build_market_update_json};

// === WebSocket Message Types ===

#[derive(Deserialize, Debug)]
pub struct BookSnapshot {
    pub asset_id: String,
    #[allow(dead_code)]
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

#[derive(Deserialize, Debug)]
pub struct PriceLevel {
    pub price: String,
    pub size: String,
}

#[derive(Deserialize, Debug)]
pub struct PriceChangeEvent {
    /// Array of price changes - the primary content of this message type.
    /// Polymarket price_change messages don't have an event_type field,
    /// they're identified by having this price_changes array.
    #[serde(default)]
    pub price_changes: Option<Vec<PriceChangeItem>>,
}

#[derive(Deserialize, Debug)]
pub struct PriceChangeItem {
    pub asset_id: String,
    pub price: Option<String>,
    pub side: Option<String>,
}

#[derive(Serialize)]
struct SubscribeCmd {
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    sub_type: &'static str,
}

// === Gamma API Client ===

pub struct GammaClient {
    http: reqwest::Client,
}

impl Default for GammaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GammaClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    /// Batch lookup multiple Polymarket markets by slug.
    /// Returns HashMap keyed by slug with (token1, token2, outcomes, neg_risk) values.
    /// Slugs not found or inactive are simply not included in the result.
    ///
    /// This is much more efficient than individual lookups:
    /// - 100 slugs = 1-2 HTTP requests instead of 100
    /// - Reduces Cloudflare rate limiting triggers
    pub async fn lookup_markets_batch(
        &self,
        slugs: &[String],
    ) -> Result<std::collections::HashMap<String, (String, String, Vec<String>, bool)>> {
        use std::collections::HashMap;

        if slugs.is_empty() {
            return Ok(HashMap::new());
        }

        // Build URL with multiple slug parameters: /markets?slug=x&slug=y&slug=z
        let query_params: Vec<String> = slugs.iter()
            .map(|s| format!("slug={}", s))
            .collect();
        let url = format!("{}/markets?{}", GAMMA_API_BASE, query_params.join("&"));

        // Retry logic for transient failures (Cloudflare throttling, network issues)
        let mut last_error = None;
        for attempt in 0..3 {
            if attempt > 0 {
                // Exponential backoff: 100ms, 200ms
                tokio::time::sleep(tokio::time::Duration::from_millis(100 * (1 << (attempt - 1)))).await;
            }

            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<Vec<GammaMarket>>().await {
                        Ok(markets) => {
                            return Ok(self.parse_gamma_batch_response(markets));
                        }
                        Err(e) => {
                            last_error = Some(format!("JSON parse error: {}", e));
                            continue;
                        }
                    }
                }
                Ok(resp) => {
                    // Non-success status - log and return empty (404 means no markets found)
                    let status = resp.status();
                    if status.as_u16() != 404 {
                        tracing::warn!("Gamma batch lookup got status {}", status);
                    }
                    return Ok(HashMap::new());
                }
                Err(e) => {
                    last_error = Some(format!("Request error: {}", e));
                    continue;
                }
            }
        }

        // All retries exhausted
        if let Some(err) = last_error {
            tracing::warn!("Gamma batch lookup failed after 3 attempts: {}", err);
            anyhow::bail!("Gamma API batch failed after 3 retries: {}", err);
        }
        Ok(HashMap::new())
    }

    /// Parse batch response into HashMap keyed by slug
    fn parse_gamma_batch_response(
        &self,
        markets: Vec<GammaMarket>,
    ) -> std::collections::HashMap<String, (String, String, Vec<String>, bool)> {
        use std::collections::HashMap;

        let mut results = HashMap::new();

        for market in markets {
            // Skip inactive/closed markets
            if market.closed == Some(true) || market.active == Some(false) {
                continue;
            }

            // Need slug to key the result
            let Some(slug) = market.slug else {
                continue;
            };

            // Parse clobTokenIds JSON array
            let token_ids: Vec<String> = market.clob_token_ids
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            // Parse outcomes JSON array
            let outcomes: Vec<String> = market.outcomes
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            // Extract neg_risk - warn if missing since this could cause wrong contract address
            let neg_risk = match market.neg_risk {
                Some(nr) => nr,
                None => {
                    tracing::warn!(
                        "[DISCOVERY] Market {} missing neg_risk field - defaulting to false (may cause order failures if actually neg_risk)",
                        slug
                    );
                    false
                }
            };

            if token_ids.len() >= 2 {
                results.insert(slug, (token_ids[0].clone(), token_ids[1].clone(), outcomes, neg_risk));
            }
        }

        results
    }

    /// Fetch events by Polymarket series ID (for esports discovery)
    pub async fn fetch_events_by_series(&self, series_id: &str) -> Result<Vec<PolyEvent>> {
        let url = format!(
            "{}/events?series_id={}&closed=false&limit=100",
            GAMMA_API_BASE, series_id
        );

        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            return Ok(vec![]);
        }

        let events: Vec<PolyEvent> = resp.json().await?;
        Ok(events)
    }
}

#[derive(Debug, Deserialize)]
struct GammaMarket {
    /// Market slug, used for matching in batch lookups
    slug: Option<String>,
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<String>,
    /// JSON array of outcome names, e.g. '["Team A", "Team B"]'
    /// Order corresponds to clob_token_ids array
    outcomes: Option<String>,
    active: Option<bool>,
    closed: Option<bool>,
    /// Whether this market uses negative risk (determines exchange contract address)
    #[serde(rename = "negRisk")]
    neg_risk: Option<bool>,
}

/// Polymarket event from /events endpoint (for esports discovery)
#[derive(Debug, Deserialize)]
pub struct PolyEvent {
    pub slug: Option<String>,
    pub title: Option<String>,
    pub markets: Option<Vec<PolyEventMarket>>,
    /// Whether this event uses negative risk (determines exchange contract address)
    #[serde(rename = "negRisk")]
    pub neg_risk: Option<bool>,
}

/// Market within a Polymarket event
#[derive(Debug, Deserialize)]
pub struct PolyEventMarket {
    pub slug: Option<String>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    /// JSON array of outcome names, e.g. '["Team A", "Team B"]'
    /// Order corresponds to clob_token_ids array
    pub outcomes: Option<String>,
}

/// Increment the date in a Polymarket slug by 1 day
/// e.g., "epl-che-avl-2025-12-08" -> "epl-che-avl-2025-12-09"
pub fn increment_date_in_slug(slug: &str) -> Option<String> {
    let parts: Vec<&str> = slug.split('-').collect();
    if parts.len() < 6 {
        return None;
    }
    
    let year: i32 = parts[3].parse().ok()?;
    let month: u32 = parts[4].parse().ok()?;
    let day: u32 = parts[5].parse().ok()?;
    
    // Compute next day
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 29 } else { 28 },
        _ => 31,
    };
    
    let (new_year, new_month, new_day) = if day >= days_in_month {
        if month == 12 { (year + 1, 1, 1) } else { (year, month + 1, 1) }
    } else {
        (year, month, day + 1)
    };
    
    // Rebuild slug with owned strings
    let prefix = parts[..3].join("-");
    let suffix = if parts.len() > 6 { format!("-{}", parts[6..].join("-")) } else { String::new() };

    Some(format!("{}-{}-{:02}-{:02}{}", prefix, new_year, new_month, new_day, suffix))
}

// =============================================================================
// WebSocket Runner
// =============================================================================

/// Parse size from Polymarket (format: "123.45" dollars)
#[inline(always)]
fn parse_size(s: &str) -> SizeCents {
    // Parse as f64 and convert to cents
    s.parse::<f64>()
        .map(|size| (size * 100.0).round() as SizeCents)
        .unwrap_or(0)
}

/// WebSocket coordinator - spawns multiple connections for large token sets.
///
/// Polymarket's WebSocket API silently fails when subscribing to >500 tokens on a single
/// connection (see [`POLY_MAX_TOKENS_PER_WS`]). This function splits the token list across
/// multiple parallel connections to work around this limitation.
///
/// Each connection manages its own reconnection logic. If any connection exits (error or
/// shutdown signal), all connections are aborted to trigger a coordinated reconnect.
pub async fn run_ws(
    state: Arc<GlobalState>,
    exec_tx: mpsc::Sender<ArbOpportunity>,
    confirm_tx: mpsc::Sender<(ArbOpportunity, Arc<MarketPair>)>,
    threshold_cents: PriceCents,
    shutdown_rx: watch::Receiver<bool>,
    clock: Arc<NanoClock>,
    tui_state: Arc<tokio::sync::RwLock<crate::confirm_tui::TuiState>>,
    log_tx: mpsc::Sender<String>,
    debug: Option<DebugBroadcaster>,
) -> Result<()> {
    // Helper to route logs based on TUI state
    let log_info = |msg: &str, tui_active: bool| {
        if tui_active {
            let _ = log_tx.try_send(format!("[{}]  INFO {}", chrono::Local::now().format("%H:%M:%S"), msg));
        } else {
            info!("{}", msg);
        }
    };

    // Collect all tokens from markets
    let tokens: Vec<String> = state.markets.iter()
        .take(state.market_count())
        .filter_map(|m| m.pair())
        .flat_map(|p| {
            tracing::debug!(
                "[POLY] Market tokens: desc={} YES={}... NO={}...",
                &p.description[..p.description.len().min(30)],
                &p.poly_yes_token[..p.poly_yes_token.len().min(16)],
                &p.poly_no_token[..p.poly_no_token.len().min(16)]
            );
            [p.poly_yes_token.to_string(), p.poly_no_token.to_string()]
        })
        .collect();

    let tui_active = tui_state.read().await.active;
    if tokens.is_empty() {
        log_info("[POLY] No markets to monitor", tui_active);
        tokio::time::sleep(Duration::from_secs(u64::MAX)).await;
        return Ok(());
    }

    // Split tokens into chunks for multiple connections
    let num_connections = (tokens.len() + POLY_MAX_TOKENS_PER_WS - 1) / POLY_MAX_TOKENS_PER_WS;

    if num_connections == 1 {
        // Single connection - run directly
        return run_single_ws(
            0,
            tokens,
            state,
            exec_tx,
            confirm_tx,
            threshold_cents,
            shutdown_rx,
            clock,
            tui_state,
            log_tx,
            debug,
        ).await;
    }

    log_info(
        &format!("[POLY] Splitting {} tokens across {} connections (max {} per connection)",
            tokens.len(),
            num_connections,
            POLY_MAX_TOKENS_PER_WS),
        tui_active,
    );

    // Spawn tasks for each chunk
    let mut handles = Vec::with_capacity(num_connections);

    for (conn_id, chunk) in tokens.chunks(POLY_MAX_TOKENS_PER_WS).enumerate() {
        let token_start = conn_id * POLY_MAX_TOKENS_PER_WS;
        let token_end = token_start + chunk.len().saturating_sub(1);
        log_info(
            &format!("[POLY:{}] Assigned tokens {}-{} ({} tokens)",
                conn_id,
                token_start,
                token_end,
                chunk.len()),
            tui_active,
        );

        let chunk_tokens = chunk.to_vec();
        let state = Arc::clone(&state);
        let exec_tx = exec_tx.clone();
        let confirm_tx = confirm_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        let clock = Arc::clone(&clock);
        let tui_state = Arc::clone(&tui_state);
        let log_tx = log_tx.clone();
        let debug = debug.clone();

        let handle = tokio::spawn(async move {
            run_single_ws(
                conn_id,
                chunk_tokens,
                state,
                exec_tx,
                confirm_tx,
                threshold_cents,
                shutdown_rx,
                clock,
                tui_state,
                log_tx,
                debug,
            ).await
        });

        handles.push(handle);
    }

    // Wait for any connection to exit
    let (result, _index, remaining) = futures_util::future::select_all(handles).await;

    // Abort remaining connections
    for handle in remaining {
        handle.abort();
    }

    // Propagate the result (or error if task panicked)
    match result {
        Ok(inner_result) => inner_result,
        Err(join_error) => Err(anyhow::anyhow!("Connection task panicked: {}", join_error)),
    }
}

/// Single WebSocket connection handler
async fn run_single_ws(
    conn_id: usize,
    tokens: Vec<String>,
    state: Arc<GlobalState>,
    exec_tx: mpsc::Sender<ArbOpportunity>,
    confirm_tx: mpsc::Sender<(ArbOpportunity, Arc<MarketPair>)>,
    threshold_cents: PriceCents,
    mut shutdown_rx: watch::Receiver<bool>,
    clock: Arc<NanoClock>,
    tui_state: Arc<tokio::sync::RwLock<crate::confirm_tui::TuiState>>,
    log_tx: mpsc::Sender<String>,
    debug: Option<DebugBroadcaster>,
) -> Result<()> {
    let log_prefix = if conn_id == 0 && tokens.len() <= POLY_MAX_TOKENS_PER_WS {
        "[POLY]".to_string()
    } else {
        format!("[POLY:{}]", conn_id)
    };

    // Helper to route logs based on TUI state
    let log_info = |msg: &str, tui_active: bool| {
        if tui_active {
            let _ = log_tx.try_send(format!("[{}]  INFO {}", chrono::Local::now().format("%H:%M:%S"), msg));
        } else {
            info!("{}", msg);
        }
    };
    let log_warn = |msg: &str, tui_active: bool| {
        if tui_active {
            let _ = log_tx.try_send(format!("[{}]  WARN {}", chrono::Local::now().format("%H:%M:%S"), msg));
        } else {
            warn!("{}", msg);
        }
    };
    let log_error = |msg: &str, tui_active: bool| {
        if tui_active {
            let _ = log_tx.try_send(format!("[{}] ERROR {}", chrono::Local::now().format("%H:%M:%S"), msg));
        } else {
            error!("{}", msg);
        }
    };

    let (ws_stream, _) = connect_async(POLYMARKET_WS_URL)
        .await
        .context("Failed to connect to Polymarket")?;

    let tui_active = tui_state.read().await.active;
    log_info(&format!("{} Connected", log_prefix), tui_active);

    let (mut write, mut read) = ws_stream.split();

    // Subscribe
    let subscribe_msg = SubscribeCmd {
        assets_ids: tokens.clone(),
        sub_type: "market",
    };

    write.send(Message::Text(serde_json::to_string(&subscribe_msg)?)).await?;
    let tui_active = tui_state.read().await.active;
    log_info(&format!("{} Subscribed to {} tokens", log_prefix, tokens.len()), tui_active);

    let mut ping_interval = interval(Duration::from_secs(POLY_PING_INTERVAL_SECS));
    let mut last_message = Instant::now();

    loop {
        tokio::select! {
            biased;

            // Check shutdown signal first
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let tui_active = tui_state.read().await.active;
                    log_info(&format!("{} Shutdown signal received, disconnecting...", log_prefix), tui_active);
                    break;
                }
            }

            _ = ping_interval.tick() => {
                if let Err(e) = write.send(Message::Ping(vec![])).await {
                    let tui_active = tui_state.read().await.active;
                    log_error(&format!("{} Failed to send ping: {}", log_prefix, e), tui_active);
                    break;
                }
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_message = Instant::now();

                        // Log raw message for debugging (first 200 chars)
                        tracing::trace!("{} Raw WS: {}...", log_prefix, &text[..text.len().min(200)]);

                        // Try book snapshot first (array of book objects)
                        if let Ok(books) = serde_json::from_str::<Vec<BookSnapshot>>(&text) {
                            if !books.is_empty() {
                                for book in &books {
                                    tracing::debug!(
                                        "{} Book snapshot: asset={} asks={} bids={}",
                                        log_prefix,
                                        &book.asset_id[..book.asset_id.len().min(20)],
                                        book.asks.len(),
                                        book.bids.len()
                                    );
                                    process_book(&state, book, &exec_tx, &confirm_tx, threshold_cents, &clock, debug.as_ref()).await;
                                }
                            }
                            // Even if empty, this was a valid book array, don't try other parsers
                        }
                        // Try price change event (single object with price_changes array)
                        // Note: Polymarket price_change messages don't have event_type field,
                        // they just have market + price_changes array directly
                        else if let Ok(event) = serde_json::from_str::<PriceChangeEvent>(&text) {
                            if let Some(changes) = &event.price_changes {
                                for change in changes {
                                    process_price_change(&state, change, &exec_tx, &confirm_tx, threshold_cents, &clock, debug.as_ref()).await;
                                }
                            }
                        }
                        // Log unknown message types at trace level for debugging
                        else {
                            tracing::trace!("{} Unknown WS message: {}...", log_prefix, &text[..text.len().min(100)]);
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if let Err(e) = write.send(Message::Pong(data)).await {
                            let tui_active = tui_state.read().await.active;
                            log_warn(&format!("{} Failed to send pong: {} (connection may be degraded)", log_prefix, e), tui_active);
                        }
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let tui_active = tui_state.read().await.active;
                        log_warn(&format!("{} Server closed: {:?}", log_prefix, frame), tui_active);
                        break;
                    }
                    Some(Err(e)) => {
                        let tui_active = tui_state.read().await.active;
                        log_error(&format!("{} WebSocket error: {}", log_prefix, e), tui_active);
                        break;
                    }
                    None => {
                        let tui_active = tui_state.read().await.active;
                        log_warn(&format!("{} Stream ended", log_prefix), tui_active);
                        break;
                    }
                    _ => {}
                }
            }
        }

        if last_message.elapsed() > Duration::from_secs(120) {
            let tui_active = tui_state.read().await.active;
            log_warn(&format!("{} Stale connection, reconnecting...", log_prefix), tui_active);
            break;
        }
    }

    Ok(())
}

/// Process book snapshot
#[inline]
async fn process_book(
    state: &GlobalState,
    book: &BookSnapshot,
    exec_tx: &mpsc::Sender<ArbOpportunity>,
    confirm_tx: &mpsc::Sender<(ArbOpportunity, Arc<MarketPair>)>,
    _threshold_cents: PriceCents,
    clock: &NanoClock,
    debug: Option<&DebugBroadcaster>,
) {
    let token_hash = fxhash_str(&book.asset_id);

    // Debug: log raw orderbook data
    let asks_debug: Vec<(u16, u16)> = book.asks.iter()
        .filter_map(|l| {
            let price = parse_price(&l.price);
            let size = parse_size(&l.size);
            if price > 0 { Some((price, size)) } else { None }
        })
        .collect();

    // Find best ask (lowest price)
    let (best_ask, ask_size) = asks_debug.iter()
        .min_by_key(|(p, _)| *p)
        .copied()
        .unwrap_or((0, 0));

    // A token can be YES for one market AND NO for another (e.g., esports where
    // Kalshi has separate markets for each team winning the same match).
    // We must check BOTH lookups, not return early.

    let mut matched = false;

    // Check if YES token
    let yes_market_id = state.poly_yes_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = yes_market_id {
        let market = &state.markets[market_id as usize];
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        // Debug: log raw asks and computed best ask
        tracing::debug!(
            "[POLY-SNAP] {} | YES asks: {:?} | best_yes_ask: {}¢ size={}",
            ticker, asks_debug, best_ask, ask_size
        );

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        market.poly.update_yes(best_ask, ask_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        // Check arbs using ArbOpportunity::detect()
        if let Some(req) = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            clock.now_ns(),
        ) {
            route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
        }
        matched = true;
    }

    // Check if NO token (same token can be NO for a different market)
    let no_market_id = state.poly_no_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = no_market_id {
        let market = &state.markets[market_id as usize];
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        // Debug: log raw asks and computed best ask
        tracing::debug!(
            "[POLY-SNAP] {} | NO asks: {:?} | best_no_ask: {}¢ size={}",
            ticker, asks_debug, best_ask, ask_size
        );

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        market.poly.update_no(best_ask, ask_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        // Check arbs using ArbOpportunity::detect()
        if let Some(req) = ArbOpportunity::detect(
            market_id,
            market.kalshi.load(),
            market.poly.load(),
            state.arb_config(),
            clock.now_ns(),
        ) {
            route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
        }
        matched = true;
    }

    if !matched {
        // Token not found in our lookup maps
        tracing::debug!(
            "[POLY] UNMATCHED token: asset={}...",
            &book.asset_id[..book.asset_id.len().min(20)]
        );
    }
}

/// Process price change
#[inline]
async fn process_price_change(
    state: &GlobalState,
    change: &PriceChangeItem,
    exec_tx: &mpsc::Sender<ArbOpportunity>,
    confirm_tx: &mpsc::Sender<(ArbOpportunity, Arc<MarketPair>)>,
    _threshold_cents: PriceCents,
    clock: &NanoClock,
    debug: Option<&DebugBroadcaster>,
) {
    // Only process SELL (ask) side updates
    // Polymarket uses "SELL" for asks and "BUY" for bids
    if !matches!(change.side.as_deref(), Some("SELL" | "sell")) {
        return;
    }

    let Some(price_str) = &change.price else { return };
    let price = parse_price(price_str);
    if price == 0 { return; }

    let token_hash = fxhash_str(&change.asset_id);

    // A token can be YES for one market AND NO for another (e.g., esports where
    // Kalshi has separate markets for each team winning the same match).
    // We must check BOTH lookups, not return early.

    // Check YES token
    let yes_market_id = state.poly_yes_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = yes_market_id {
        let market = &state.markets[market_id as usize];
        let (current_yes, current_no, current_yes_size, _) = market.poly.load();
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Always update cached price to reflect current market state
        market.poly.update_yes(price, current_yes_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        // Debug: log price change
        tracing::debug!(
            "[POLY-DELTA] {} | YES: yes_ask={}¢ no_ask={}¢ (was yes={}¢ no={}¢)",
            ticker, price, current_no, current_yes, current_no
        );

        // Only check arbs when price improves (lower = better for buying)
        if price < current_yes || current_yes == 0 {
            if let Some(req) = ArbOpportunity::detect(
                market_id,
                market.kalshi.load(),
                market.poly.load(),
                state.arb_config(),
                clock.now_ns(),
            ) {
                route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
            }
        }
    }

    // Check NO token (same token can be NO for a different market)
    let no_market_id = state.poly_no_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = no_market_id {
        let market = &state.markets[market_id as usize];
        let (current_yes, current_no, _, current_no_size) = market.poly.load();
        let ticker = market.pair()
            .map(|p| p.kalshi_market_ticker.to_string())
            .unwrap_or_else(|| format!("market_{}", market_id));

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Always update cached price to reflect current market state
        market.poly.update_no(price, current_no_size);
        market.mark_poly_update_unix_ms(now_ms);
        market.inc_poly_updates();
        if let Some(dbg) = debug {
            if let Some(json) = build_market_update_json(state, market_id) {
                dbg.send_json(json);
            }
        }

        // Debug: log price change
        tracing::debug!(
            "[POLY-DELTA] {} | NO: yes_ask={}¢ no_ask={}¢ (was yes={}¢ no={}¢)",
            ticker, current_yes, price, current_yes, current_no
        );

        // Only check arbs when price improves (lower = better for buying)
        if price < current_no || current_no == 0 {
            if let Some(req) = ArbOpportunity::detect(
                market_id,
                market.kalshi.load(),
                market.poly.load(),
                state.arb_config(),
                clock.now_ns(),
            ) {
                route_arb_to_channel(state, market_id, req, exec_tx, confirm_tx).await;
            }
        }
    }
}

/// Route a detected arbitrage opportunity to the appropriate channel.
///
/// Routes to confirm_tx if the league requires confirmation, otherwise to exec_tx.
#[inline]
async fn route_arb_to_channel(
    state: &GlobalState,
    market_id: u16,
    req: ArbOpportunity,
    exec_tx: &mpsc::Sender<ArbOpportunity>,
    confirm_tx: &mpsc::Sender<(ArbOpportunity, Arc<MarketPair>)>,
) {
    // Get market pair to check if confirmation is required
    let pair = match state.get_by_id(market_id).and_then(|m| m.pair()) {
        Some(p) => p,
        None => {
            // No pair found, send directly to exec channel
            if let Err(e) = exec_tx.try_send(req) {
                tracing::warn!(
                    "[POLY] Arb request dropped for market {}: {} (channel backpressure)",
                    market_id, e
                );
            }
            return;
        }
    };

    // Route based on confirmation requirement
    if config::requires_confirmation(&pair.league) {
        if let Err(e) = confirm_tx.try_send((req, pair)) {
            tracing::warn!(
                "[POLY] Confirm request dropped for market {}: {} (channel backpressure)",
                market_id, e
            );
        }
    } else {
        if let Err(e) = exec_tx.try_send(req) {
            tracing::warn!(
                "[POLY] Arb request dropped for market {}: {} (channel backpressure)",
                market_id, e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::{GlobalState, MarketPair, MarketType, fxhash_str};

    /// Test helper: Create a GlobalState with esports-style market pairs where
    /// the same Polymarket tokens are used across multiple Kalshi markets.
    ///
    /// Production example from COD (Call of Duty League Stage 1):
    /// - Polymarket: "OpTic Texas vs LA Thieves" with two tokens:
    ///   - Token_A (43216923243873086858): "OpTic Texas wins"
    ///   - Token_B (38935270389103557597): "LA Thieves wins"
    ///
    /// - Kalshi has TWO markets for the same match:
    ///   - Market 1: "Will OpTic Texas win?"
    ///     → poly_yes = Token_A (OpTic wins), poly_no = Token_B (OpTic loses = Thieves wins)
    ///   - Market 2: "Will LA Thieves win?"
    ///     → poly_yes = Token_B (Thieves wins), poly_no = Token_A (Thieves loses = OpTic wins)
    ///
    /// This means Token_A is:
    ///   - YES for Market 1 (OpTic winning)
    ///   - NO for Market 2 (Thieves losing)
    ///
    /// And Token_B is:
    ///   - YES for Market 2 (Thieves winning)
    ///   - NO for Market 1 (OpTic losing)
    fn create_esports_state_with_shared_tokens() -> (GlobalState, String, String) {
        let state = GlobalState::default();

        // Real token IDs from production logs (truncated for readability in tests)
        let token_optic = "43216923243873086858".to_string();  // OpTic Texas wins
        let token_thieves = "38935270389103557597".to_string(); // LA Thieves wins

        // Market 1: "Will OpTic Texas win?"
        // YES = OpTic wins (token_optic), NO = OpTic loses = Thieves wins (token_thieves)
        let pair1 = MarketPair {
            pair_id: "cod-optic-thieves-OPTIC".into(),
            league: "cod".into(),
            market_type: MarketType::Moneyline,
            description: "COD Stage 1 - Will OpTic Texas win?".into(),
            kalshi_event_ticker: "KXCODGAME-26JAN17OPTICTHIEVES".into(),
            kalshi_market_ticker: "KXCODGAME-26JAN17OPTICTHIEVES-OPTIC".into(),
            kalshi_event_slug: "call-of-duty-game".into(),
            poly_slug: "codmw-optic-thieves-2026-01-17".into(),
            poly_yes_token: token_optic.clone().into(),
            poly_no_token: token_thieves.clone().into(),
            line_value: None,
            team_suffix: Some("OPTIC".into()),
            neg_risk: false,
        };

        // Market 2: "Will LA Thieves win?"
        // YES = Thieves wins (token_thieves), NO = Thieves loses = OpTic wins (token_optic)
        // Note: tokens are SWAPPED compared to Market 1
        let pair2 = MarketPair {
            pair_id: "cod-optic-thieves-THIEVES".into(),
            league: "cod".into(),
            market_type: MarketType::Moneyline,
            description: "COD Stage 1 - Will LA Thieves win?".into(),
            kalshi_event_ticker: "KXCODGAME-26JAN17OPTICTHIEVES".into(),
            kalshi_market_ticker: "KXCODGAME-26JAN17OPTICTHIEVES-THIEVES".into(),
            kalshi_event_slug: "call-of-duty-game".into(),
            poly_slug: "codmw-optic-thieves-2026-01-17".into(),
            poly_yes_token: token_thieves.clone().into(),  // SWAPPED
            poly_no_token: token_optic.clone().into(),     // SWAPPED
            line_value: None,
            team_suffix: Some("THIEVES".into()),
            neg_risk: false,
        };

        state.add_pair(pair1);
        state.add_pair(pair2);

        (state, token_optic, token_thieves)
    }

    #[test]
    fn test_shared_token_lookup_structure() {
        // Verify the lookup maps correctly store tokens that appear in both YES and NO roles
        let (state, token_optic, token_thieves) = create_esports_state_with_shared_tokens();

        let optic_hash = fxhash_str(&token_optic);
        let thieves_hash = fxhash_str(&token_thieves);

        // Token_OpTic should be in BOTH maps (YES for market 0, NO for market 1)
        let optic_yes_market = state.poly_yes_to_id.read().get(&optic_hash).copied();
        let optic_no_market = state.poly_no_to_id.read().get(&optic_hash).copied();

        assert_eq!(optic_yes_market, Some(0), "Token_OpTic should be YES for market 0");
        assert_eq!(optic_no_market, Some(1), "Token_OpTic should be NO for market 1");

        // Token_Thieves should be in BOTH maps (YES for market 1, NO for market 0)
        let thieves_yes_market = state.poly_yes_to_id.read().get(&thieves_hash).copied();
        let thieves_no_market = state.poly_no_to_id.read().get(&thieves_hash).copied();

        assert_eq!(thieves_yes_market, Some(1), "Token_Thieves should be YES for market 1");
        assert_eq!(thieves_no_market, Some(0), "Token_Thieves should be NO for market 0");
    }

    #[test]
    fn test_book_snapshot_updates_both_markets() {
        // When we receive a book snapshot for Token_OpTic, it should update:
        // - Market 0's YES price (OpTic winning)
        // - Market 1's NO price (Thieves losing = OpTic winning)
        let (state, token_optic, token_thieves) = create_esports_state_with_shared_tokens();

        // Simulate book snapshot for Token_OpTic with price 34 cents, size 462 cents
        let optic_hash = fxhash_str(&token_optic);
        let optic_price: u16 = 34;
        let optic_size: u16 = 462;

        // Simulate what process_book does (without the async/exec_tx parts)
        // First check YES lookup
        if let Some(market_id) = state.poly_yes_to_id.read().get(&optic_hash).copied() {
            state.markets[market_id as usize].poly.update_yes(optic_price, optic_size);
            state.markets[market_id as usize].inc_poly_updates();
        }

        // Then check NO lookup (same token can be NO for different market)
        if let Some(market_id) = state.poly_no_to_id.read().get(&optic_hash).copied() {
            state.markets[market_id as usize].poly.update_no(optic_price, optic_size);
            state.markets[market_id as usize].inc_poly_updates();
        }

        // Verify Market 0: Token_OpTic is YES, should have YES price updated
        let (m0_yes, m0_no, _, _) = state.markets[0].poly.load();
        assert_eq!(m0_yes, 34, "Market 0 YES (OpTic wins) should be 34 cents");
        assert_eq!(m0_no, 0, "Market 0 NO should still be 0 (waiting for Token_Thieves)");

        // Verify Market 1: Token_OpTic is NO, should have NO price updated
        let (m1_yes, m1_no, _, _) = state.markets[1].poly.load();
        assert_eq!(m1_yes, 0, "Market 1 YES should still be 0 (waiting for Token_Thieves)");
        assert_eq!(m1_no, 34, "Market 1 NO (Thieves loses) should be 34 cents");

        // Now simulate book snapshot for Token_Thieves with price 70 cents
        let thieves_hash = fxhash_str(&token_thieves);
        let thieves_price: u16 = 70;
        let thieves_size: u16 = 2675;

        if let Some(market_id) = state.poly_yes_to_id.read().get(&thieves_hash).copied() {
            state.markets[market_id as usize].poly.update_yes(thieves_price, thieves_size);
            state.markets[market_id as usize].inc_poly_updates();
        }

        if let Some(market_id) = state.poly_no_to_id.read().get(&thieves_hash).copied() {
            state.markets[market_id as usize].poly.update_no(thieves_price, thieves_size);
            state.markets[market_id as usize].inc_poly_updates();
        }

        // Now both markets should have complete pricing
        let (m0_yes, m0_no, _, _) = state.markets[0].poly.load();
        assert_eq!(m0_yes, 34, "Market 0 YES (OpTic wins) = 34 cents");
        assert_eq!(m0_no, 70, "Market 0 NO (OpTic loses) = 70 cents");

        let (m1_yes, m1_no, _, _) = state.markets[1].poly.load();
        assert_eq!(m1_yes, 70, "Market 1 YES (Thieves wins) = 70 cents");
        assert_eq!(m1_no, 34, "Market 1 NO (Thieves loses) = 34 cents");

        // Verify prices are complementary: YES + NO should be close to 100 cents
        // (In this case 34 + 70 = 104, slightly over due to market spread)
        assert_eq!(m0_yes + m0_no, 104);
        assert_eq!(m1_yes + m1_no, 104);

        // Verify update counts: each market got 2 updates (one from each token)
        let (m0_k_upd, m0_p_upd) = state.markets[0].load_update_counts();
        let (m1_k_upd, m1_p_upd) = state.markets[1].load_update_counts();
        assert_eq!(m0_p_upd, 2, "Market 0 should have 2 Poly updates");
        assert_eq!(m1_p_upd, 2, "Market 1 should have 2 Poly updates");
        assert_eq!(m0_k_upd, 0, "Market 0 should have 0 Kalshi updates");
        assert_eq!(m1_k_upd, 0, "Market 1 should have 0 Kalshi updates");
    }

    #[test]
    fn test_early_return_bug_regression() {
        // This test verifies the bug fix: the old code had `return` after YES lookup,
        // which prevented NO lookup from running. This caused Market 1's NO price
        // to never be updated when Token_OpTic's book snapshot arrived.
        //
        // Production symptom: verbose heartbeat showed P:--/-- for all markets even
        // though book snapshots were being received and matched as YES.
        let (state, token_optic, _token_thieves) = create_esports_state_with_shared_tokens();

        let optic_hash = fxhash_str(&token_optic);
        let price: u16 = 34;
        let size: u16 = 462;

        // OLD BUGGY CODE (don't do this):
        // if let Some(market_id) = state.poly_yes_to_id.read().get(&optic_hash).copied() {
        //     state.markets[market_id as usize].poly.update_yes(price, size);
        //     return;  // <-- BUG: early return prevents NO lookup
        // }
        // if let Some(market_id) = state.poly_no_to_id.read().get(&optic_hash).copied() {
        //     // This never runs because of early return above!
        //     state.markets[market_id as usize].poly.update_no(price, size);
        // }

        // FIXED CODE (what we do now):
        // Check YES lookup
        if let Some(market_id) = state.poly_yes_to_id.read().get(&optic_hash).copied() {
            state.markets[market_id as usize].poly.update_yes(price, size);
            // NO return here - continue to check NO lookup
        }
        // Check NO lookup (same token can be NO for different market)
        if let Some(market_id) = state.poly_no_to_id.read().get(&optic_hash).copied() {
            state.markets[market_id as usize].poly.update_no(price, size);
        }

        // Both markets should be updated
        let (m0_yes, _, _, _) = state.markets[0].poly.load();
        let (_, m1_no, _, _) = state.markets[1].poly.load();

        assert_eq!(m0_yes, 34, "Market 0 YES should be updated");
        assert_eq!(m1_no, 34, "Market 1 NO should ALSO be updated (regression test)");
    }

    #[test]
    fn test_production_token_ids_from_debug_logs() {
        // Real production data from debug logs:
        // [POLY] Market tokens: desc=Call of Duty League Stage 1 Ma YES=4321692324387308... NO=3893527038910355...
        // [POLY] Market tokens: desc=Call of Duty League Stage 1 Ma YES=3893527038910355... NO=4321692324387308...
        //
        // Book snapshots received:
        // [POLY] Book snapshot: asset=43216923243873086858 asks=16 bids=13
        // [POLY] YES matched: asset=4321692324387308... price=34 size=462
        // [POLY] Book snapshot: asset=38935270389103557597 asks=13 bids=16
        // [POLY] YES matched: asset=3893527038910355... price=70 size=2675
        //
        // With the bug, only YES matches were logged, NO matches never happened.
        // After fix, both YES and NO matches should occur for each book snapshot.

        let state = GlobalState::default();

        // Exact token IDs from production
        let token_a = "43216923243873086858".to_string();
        let token_b = "38935270389103557597".to_string();

        // Market pair 1: YES=token_a, NO=token_b
        let pair1 = MarketPair {
            pair_id: "test-pair-1".into(),
            league: "cod".into(),
            market_type: MarketType::Moneyline,
            description: "Call of Duty League Stage 1 Match - Team A".into(),
            kalshi_event_ticker: "KXCODGAME-26JAN17AB".into(),
            kalshi_market_ticker: "KXCODGAME-26JAN17AB-A".into(),
            kalshi_event_slug: "call-of-duty-game".into(),
            poly_slug: "codmw-a-b-2026-01-17".into(),
            poly_yes_token: token_a.clone().into(),
            poly_no_token: token_b.clone().into(),
            line_value: None,
            team_suffix: Some("A".into()),
            neg_risk: false,
        };

        // Market pair 2: YES=token_b, NO=token_a (swapped)
        let pair2 = MarketPair {
            pair_id: "test-pair-2".into(),
            league: "cod".into(),
            market_type: MarketType::Moneyline,
            description: "Call of Duty League Stage 1 Match - Team B".into(),
            kalshi_event_ticker: "KXCODGAME-26JAN17AB".into(),
            kalshi_market_ticker: "KXCODGAME-26JAN17AB-B".into(),
            kalshi_event_slug: "call-of-duty-game".into(),
            poly_slug: "codmw-a-b-2026-01-17".into(),
            poly_yes_token: token_b.clone().into(),
            poly_no_token: token_a.clone().into(),
            line_value: None,
            team_suffix: Some("B".into()),
            neg_risk: false,
        };

        state.add_pair(pair1);
        state.add_pair(pair2);

        // Simulate processing book snapshot for token_a (price=34, size=462)
        let hash_a = fxhash_str(&token_a);
        if let Some(id) = state.poly_yes_to_id.read().get(&hash_a).copied() {
            state.markets[id as usize].poly.update_yes(34, 462);
        }
        if let Some(id) = state.poly_no_to_id.read().get(&hash_a).copied() {
            state.markets[id as usize].poly.update_no(34, 462);
        }

        // Simulate processing book snapshot for token_b (price=70, size=2675)
        let hash_b = fxhash_str(&token_b);
        if let Some(id) = state.poly_yes_to_id.read().get(&hash_b).copied() {
            state.markets[id as usize].poly.update_yes(70, 2675);
        }
        if let Some(id) = state.poly_no_to_id.read().get(&hash_b).copied() {
            state.markets[id as usize].poly.update_no(70, 2675);
        }

        // Verify both markets have complete pricing
        let (m0_yes, m0_no, m0_yes_size, m0_no_size) = state.markets[0].poly.load();
        let (m1_yes, m1_no, m1_yes_size, m1_no_size) = state.markets[1].poly.load();

        // Market 0: YES=token_a(34), NO=token_b(70)
        assert_eq!(m0_yes, 34);
        assert_eq!(m0_no, 70);
        assert_eq!(m0_yes_size, 462);
        assert_eq!(m0_no_size, 2675);

        // Market 1: YES=token_b(70), NO=token_a(34)
        assert_eq!(m1_yes, 70);
        assert_eq!(m1_no, 34);
        assert_eq!(m1_yes_size, 2675);
        assert_eq!(m1_no_size, 462);
    }

    #[test]
    fn test_standard_market_single_token_per_role() {
        // Standard sports markets (NBA, NFL, etc.) don't have shared tokens.
        // Each market has unique YES/NO tokens. Verify this still works.
        let state = GlobalState::default();

        let pair = MarketPair {
            pair_id: "nba-lal-bos".into(),
            league: "nba".into(),
            market_type: MarketType::Moneyline,
            description: "Lakers vs Celtics".into(),
            kalshi_event_ticker: "KXNBAGAME-26JAN17LALBOS".into(),
            kalshi_market_ticker: "KXNBAGAME-26JAN17LALBOS-LAL".into(),
            kalshi_event_slug: "nba-game".into(),
            poly_slug: "nba-lal-bos-2026-01-17".into(),
            poly_yes_token: "unique_yes_token_123".into(),
            poly_no_token: "unique_no_token_456".into(),
            line_value: None,
            team_suffix: Some("LAL".into()),
            neg_risk: false,
        };

        state.add_pair(pair);

        // Verify tokens are only in one map each
        let yes_hash = fxhash_str("unique_yes_token_123");
        let no_hash = fxhash_str("unique_no_token_456");

        assert!(state.poly_yes_to_id.read().get(&yes_hash).is_some());
        assert!(state.poly_no_to_id.read().get(&yes_hash).is_none()); // YES token not in NO map

        assert!(state.poly_no_to_id.read().get(&no_hash).is_some());
        assert!(state.poly_yes_to_id.read().get(&no_hash).is_none()); // NO token not in YES map

        // Update prices
        if let Some(id) = state.poly_yes_to_id.read().get(&yes_hash).copied() {
            state.markets[id as usize].poly.update_yes(45, 1000);
        }
        if let Some(id) = state.poly_no_to_id.read().get(&no_hash).copied() {
            state.markets[id as usize].poly.update_no(58, 2000);
        }

        let (yes, no, yes_size, no_size) = state.markets[0].poly.load();
        assert_eq!(yes, 45);
        assert_eq!(no, 58);
        assert_eq!(yes_size, 1000);
        assert_eq!(no_size, 2000);
    }

    #[test]
    fn test_price_change_updates_both_markets_for_shared_token() {
        // Test that process_price_change logic updates both markets when a shared
        // token's price changes. This is the same bug fix as process_book but for
        // incremental price updates instead of book snapshots.
        //
        // Scenario: Esports market where Token_A is YES for Market 0, NO for Market 1.
        // When Token_A's price changes, BOTH markets should be updated.
        let (state, token_optic, _token_thieves) = create_esports_state_with_shared_tokens();

        // Simulate price change for Token_OpTic: new price = 40 cents
        // This simulates what process_price_change does (without async/exec_tx)
        let optic_hash = fxhash_str(&token_optic);
        let new_price: u16 = 40;

        // First, set initial sizes (process_price_change preserves sizes)
        state.markets[0].poly.update_yes(34, 500);  // Initial YES for market 0
        state.markets[1].poly.update_no(34, 500);   // Initial NO for market 1

        // Now simulate process_price_change receiving a price update for token_optic
        // The function checks YES lookup first, then NO lookup (no early return)

        // Check YES token lookup
        if let Some(market_id) = state.poly_yes_to_id.read().get(&optic_hash).copied() {
            let market = &state.markets[market_id as usize];
            let (_, _, current_yes_size, _) = market.poly.load();
            market.poly.update_yes(new_price, current_yes_size);
            market.inc_poly_updates();
        }

        // Check NO token lookup (same token can be NO for different market)
        // OLD BUGGY CODE would have `return` above, never reaching here
        if let Some(market_id) = state.poly_no_to_id.read().get(&optic_hash).copied() {
            let market = &state.markets[market_id as usize];
            let (_, _, _, current_no_size) = market.poly.load();
            market.poly.update_no(new_price, current_no_size);
            market.inc_poly_updates();
        }

        // Verify Market 0: Token_OpTic is YES, should have YES price updated to 40
        let (m0_yes, _, _, _) = state.markets[0].poly.load();
        assert_eq!(m0_yes, 40, "Market 0 YES (OpTic wins) should be updated to 40 cents");

        // Verify Market 1: Token_OpTic is NO, should have NO price updated to 40
        let (_, m1_no, _, _) = state.markets[1].poly.load();
        assert_eq!(m1_no, 40, "Market 1 NO (Thieves loses) should ALSO be updated to 40 cents");

        // Verify update counts reflect both markets were updated
        let (_, m0_p_upd) = state.markets[0].load_update_counts();
        let (_, m1_p_upd) = state.markets[1].load_update_counts();
        assert_eq!(m0_p_upd, 1, "Market 0 should have 1 Poly update from price change");
        assert_eq!(m1_p_upd, 1, "Market 1 should have 1 Poly update from price change");
    }

    // === Tests for increment_date_in_slug ===

    #[test]
    fn test_increment_date_in_slug_basic() {
        use super::increment_date_in_slug;

        // Basic case: mid-month
        assert_eq!(
            increment_date_in_slug("epl-che-avl-2025-12-08"),
            Some("epl-che-avl-2025-12-09".to_string())
        );

        // NBA style slug
        assert_eq!(
            increment_date_in_slug("nba-lal-bos-2026-01-17"),
            Some("nba-lal-bos-2026-01-18".to_string())
        );
    }

    #[test]
    fn test_increment_date_in_slug_month_rollover() {
        use super::increment_date_in_slug;

        // End of 31-day month
        assert_eq!(
            increment_date_in_slug("nba-lal-bos-2026-01-31"),
            Some("nba-lal-bos-2026-02-01".to_string())
        );

        // End of 30-day month
        assert_eq!(
            increment_date_in_slug("epl-mun-liv-2026-04-30"),
            Some("epl-mun-liv-2026-05-01".to_string())
        );
    }

    #[test]
    fn test_increment_date_in_slug_year_rollover() {
        use super::increment_date_in_slug;

        // End of year
        assert_eq!(
            increment_date_in_slug("nfl-dal-phi-2025-12-31"),
            Some("nfl-dal-phi-2026-01-01".to_string())
        );
    }

    #[test]
    fn test_increment_date_in_slug_february() {
        use super::increment_date_in_slug;

        // Non-leap year February
        assert_eq!(
            increment_date_in_slug("nba-lal-bos-2025-02-28"),
            Some("nba-lal-bos-2025-03-01".to_string())
        );

        // Leap year February
        assert_eq!(
            increment_date_in_slug("nba-lal-bos-2024-02-28"),
            Some("nba-lal-bos-2024-02-29".to_string())
        );
        assert_eq!(
            increment_date_in_slug("nba-lal-bos-2024-02-29"),
            Some("nba-lal-bos-2024-03-01".to_string())
        );
    }

    #[test]
    fn test_increment_date_in_slug_with_suffix() {
        use super::increment_date_in_slug;

        // Spread market with suffix
        assert_eq!(
            increment_date_in_slug("nba-lal-bos-2026-01-17-spread-home-5pt5"),
            Some("nba-lal-bos-2026-01-18-spread-home-5pt5".to_string())
        );

        // Total market with suffix
        assert_eq!(
            increment_date_in_slug("nba-lal-bos-2026-01-31-total-220pt5"),
            Some("nba-lal-bos-2026-02-01-total-220pt5".to_string())
        );
    }

    #[test]
    fn test_increment_date_in_slug_invalid() {
        use super::increment_date_in_slug;

        // Too few parts
        assert_eq!(increment_date_in_slug("nba-lal-bos"), None);
        assert_eq!(increment_date_in_slug("invalid"), None);

        // Invalid date parts
        assert_eq!(increment_date_in_slug("nba-lal-bos-abc-01-17"), None);
    }
}