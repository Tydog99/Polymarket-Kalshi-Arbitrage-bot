//! Polymarket platform integration client.
//!
//! This module provides WebSocket client for real-time Polymarket price feeds
//! and REST API client for market discovery via the Gamma API.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::config::{POLYMARKET_WS_URL, POLY_PING_INTERVAL_SECS, GAMMA_API_BASE, POLY_MAX_TOKENS_PER_WS};
use crate::execution::NanoClock;
use crate::types::{
    GlobalState, FastExecutionRequest, ArbType, PriceCents, SizeCents,
    parse_price, fxhash_str,
};

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
    pub event_type: Option<String>,
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
    
    /// Look up Polymarket market by slug, return (token1, token2, outcomes)
    /// Tries both the exact date and next day (timezone handling)
    /// Note: token1/token2 are in Gamma API order; caller must use outcomes to determine YES/NO
    pub async fn lookup_market(&self, slug: &str) -> Result<Option<(String, String, Vec<String>)>> {
        // Try exact slug first
        if let Some(result) = self.try_lookup_slug(slug).await? {
            return Ok(Some(result));
        }

        // Try with next day (Polymarket may use local time)
        if let Some(next_day_slug) = increment_date_in_slug(slug) {
            if let Some(result) = self.try_lookup_slug(&next_day_slug).await? {
                info!("  📅 Found with next-day slug: {}", next_day_slug);
                return Ok(Some(result));
            }
        }

        Ok(None)
    }
    
    async fn try_lookup_slug(&self, slug: &str) -> Result<Option<(String, String, Vec<String>)>> {
        let url = format!("{}/markets?slug={}", GAMMA_API_BASE, slug);

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
                            // Success - continue with normal logic below
                            return self.parse_gamma_market_response(markets);
                        }
                        Err(e) => {
                            last_error = Some(format!("JSON parse error: {}", e));
                            continue;
                        }
                    }
                }
                Ok(_resp) => {
                    // Non-success status, don't retry (likely 404)
                    return Ok(None);
                }
                Err(e) => {
                    last_error = Some(format!("Request error: {}", e));
                    continue;
                }
            }
        }

        // All retries exhausted
        if let Some(err) = last_error {
            tracing::warn!("Gamma lookup failed after 3 attempts for {}: {}", slug, err);
        }
        Ok(None)
    }

    fn parse_gamma_market_response(&self, markets: Vec<GammaMarket>) -> Result<Option<(String, String, Vec<String>)>> {
        if markets.is_empty() {
            return Ok(None);
        }

        let market = &markets[0];

        // Check if active and not closed
        if market.closed == Some(true) || market.active == Some(false) {
            return Ok(None);
        }

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

        if token_ids.len() >= 2 {
            Ok(Some((token_ids[0].clone(), token_ids[1].clone(), outcomes)))
        } else {
            Ok(None)
        }
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
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<String>,
    /// JSON array of outcome names, e.g. '["Team A", "Team B"]'
    /// Order corresponds to clob_token_ids array
    outcomes: Option<String>,
    active: Option<bool>,
    closed: Option<bool>,
}

/// Polymarket event from /events endpoint (for esports discovery)
#[derive(Debug, Deserialize)]
pub struct PolyEvent {
    pub slug: Option<String>,
    pub title: Option<String>,
    pub markets: Option<Vec<PolyEventMarket>>,
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
fn increment_date_in_slug(slug: &str) -> Option<String> {
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

/// WebSocket coordinator - spawns multiple connections for large token sets
pub async fn run_ws(
    state: Arc<GlobalState>,
    exec_tx: mpsc::Sender<FastExecutionRequest>,
    threshold_cents: PriceCents,
    shutdown_rx: watch::Receiver<bool>,
    clock: Arc<NanoClock>,
) -> Result<()> {
    // Collect all tokens from markets
    let tokens: Vec<String> = state.markets.iter()
        .take(state.market_count())
        .filter_map(|m| m.pair())
        .flat_map(|p| [p.poly_yes_token.to_string(), p.poly_no_token.to_string()])
        .collect();

    if tokens.is_empty() {
        info!("[POLY] No markets to monitor");
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
            threshold_cents,
            shutdown_rx,
            clock,
        ).await;
    }

    info!(
        "[POLY] Splitting {} tokens across {} connections (max {} per connection)",
        tokens.len(),
        num_connections,
        POLY_MAX_TOKENS_PER_WS
    );

    // Spawn tasks for each chunk
    let mut handles = Vec::with_capacity(num_connections);

    for (conn_id, chunk) in tokens.chunks(POLY_MAX_TOKENS_PER_WS).enumerate() {
        let token_start = conn_id * POLY_MAX_TOKENS_PER_WS;
        let token_end = token_start + chunk.len() - 1;
        info!(
            "[POLY:{}] Assigned tokens {}-{} ({} tokens)",
            conn_id,
            token_start,
            token_end,
            chunk.len()
        );

        let chunk_tokens = chunk.to_vec();
        let state = Arc::clone(&state);
        let exec_tx = exec_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        let clock = Arc::clone(&clock);

        let handle = tokio::spawn(async move {
            run_single_ws(
                conn_id,
                chunk_tokens,
                state,
                exec_tx,
                threshold_cents,
                shutdown_rx,
                clock,
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
    exec_tx: mpsc::Sender<FastExecutionRequest>,
    threshold_cents: PriceCents,
    mut shutdown_rx: watch::Receiver<bool>,
    clock: Arc<NanoClock>,
) -> Result<()> {
    let log_prefix = if conn_id == 0 && tokens.len() <= POLY_MAX_TOKENS_PER_WS {
        "[POLY]".to_string()
    } else {
        format!("[POLY:{}]", conn_id)
    };

    let (ws_stream, _) = connect_async(POLYMARKET_WS_URL)
        .await
        .context("Failed to connect to Polymarket")?;

    info!("{} Connected", log_prefix);

    let (mut write, mut read) = ws_stream.split();

    // Subscribe
    let subscribe_msg = SubscribeCmd {
        assets_ids: tokens.clone(),
        sub_type: "market",
    };

    write.send(Message::Text(serde_json::to_string(&subscribe_msg)?)).await?;
    info!("{} Subscribed to {} tokens", log_prefix, tokens.len());

    let mut ping_interval = interval(Duration::from_secs(POLY_PING_INTERVAL_SECS));
    let mut last_message = Instant::now();

    loop {
        tokio::select! {
            biased;

            // Check shutdown signal first
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("{} Shutdown signal received, disconnecting...", log_prefix);
                    break;
                }
            }

            _ = ping_interval.tick() => {
                if let Err(e) = write.send(Message::Ping(vec![])).await {
                    error!("{} Failed to send ping: {}", log_prefix, e);
                    break;
                }
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_message = Instant::now();

                        // Try book snapshot first
                        if let Ok(books) = serde_json::from_str::<Vec<BookSnapshot>>(&text) {
                            for book in &books {
                                process_book(&state, book, &exec_tx, threshold_cents, &clock).await;
                            }
                        }
                        // Try price change event
                        else if let Ok(event) = serde_json::from_str::<PriceChangeEvent>(&text) {
                            if event.event_type.as_deref() == Some("price_change") {
                                if let Some(changes) = &event.price_changes {
                                    for change in changes {
                                        process_price_change(&state, change, &exec_tx, threshold_cents, &clock).await;
                                    }
                                }
                            }
                        }
                        // Log unknown message types at trace level for debugging
                        else {
                            tracing::trace!("{} Unknown WS message: {}...", log_prefix, &text[..text.len().min(100)]);
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Close(frame))) => {
                        warn!("{} Server closed: {:?}", log_prefix, frame);
                        break;
                    }
                    Some(Err(e)) => {
                        error!("{} WebSocket error: {}", log_prefix, e);
                        break;
                    }
                    None => {
                        warn!("{} Stream ended", log_prefix);
                        break;
                    }
                    _ => {}
                }
            }
        }

        if last_message.elapsed() > Duration::from_secs(120) {
            warn!("{} Stale connection, reconnecting...", log_prefix);
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
    exec_tx: &mpsc::Sender<FastExecutionRequest>,
    threshold_cents: PriceCents,
    clock: &NanoClock,
) {
    let token_hash = fxhash_str(&book.asset_id);

    // Find best ask (lowest price)
    let (best_ask, ask_size) = book.asks.iter()
        .filter_map(|l| {
            let price = parse_price(&l.price);
            let size = parse_size(&l.size);
            if price > 0 { Some((price, size)) } else { None }
        })
        .min_by_key(|(p, _)| *p)
        .unwrap_or((0, 0));

    // Check if YES token (release lock before any await)
    let yes_market_id = state.poly_yes_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = yes_market_id {
        let market = &state.markets[market_id as usize];
        market.poly.update_yes(best_ask, ask_size);

        // Check arbs
        let arb_mask = market.check_arbs(threshold_cents);
        if arb_mask != 0 {
            send_arb_request(market_id, market, arb_mask, exec_tx, clock).await;
        }
        return;
    }

    // Check if NO token (release lock before any await)
    let no_market_id = state.poly_no_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = no_market_id {
        let market = &state.markets[market_id as usize];
        market.poly.update_no(best_ask, ask_size);

        // Check arbs
        let arb_mask = market.check_arbs(threshold_cents);
        if arb_mask != 0 {
            send_arb_request(market_id, market, arb_mask, exec_tx, clock).await;
        }
    }
}

/// Process price change
#[inline]
async fn process_price_change(
    state: &GlobalState,
    change: &PriceChangeItem,
    exec_tx: &mpsc::Sender<FastExecutionRequest>,
    threshold_cents: PriceCents,
    clock: &NanoClock,
) {
    // Only process ASK side updates
    if !matches!(change.side.as_deref(), Some("ASK" | "ask")) {
        return;
    }

    let Some(price_str) = &change.price else { return };
    let price = parse_price(price_str);
    if price == 0 { return; }

    let token_hash = fxhash_str(&change.asset_id);

    // Check YES token (release lock before any await)
    let yes_market_id = state.poly_yes_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = yes_market_id {
        let market = &state.markets[market_id as usize];
        let (current_yes, _, current_yes_size, _) = market.poly.load();

        // Always update cached price to reflect current market state
        market.poly.update_yes(price, current_yes_size);

        // Only check arbs when price improves (lower = better for buying)
        if price < current_yes || current_yes == 0 {
            let arb_mask = market.check_arbs(threshold_cents);
            if arb_mask != 0 {
                send_arb_request(market_id, market, arb_mask, exec_tx, clock).await;
            }
        }
        return;
    }

    // Check NO token (release lock before any await)
    let no_market_id = state.poly_no_to_id.read().get(&token_hash).copied();
    if let Some(market_id) = no_market_id {
        let market = &state.markets[market_id as usize];
        let (_, current_no, _, current_no_size) = market.poly.load();

        // Always update cached price to reflect current market state
        market.poly.update_no(price, current_no_size);

        // Only check arbs when price improves (lower = better for buying)
        if price < current_no || current_no == 0 {
            let arb_mask = market.check_arbs(threshold_cents);
            if arb_mask != 0 {
                send_arb_request(market_id, market, arb_mask, exec_tx, clock).await;
            }
        }
    }
}

/// Send arb request to execution engine
#[inline]
async fn send_arb_request(
    market_id: u16,
    market: &crate::types::AtomicMarketState,
    arb_mask: u8,
    exec_tx: &mpsc::Sender<FastExecutionRequest>,
    clock: &NanoClock,
) {
    let (k_yes, k_no, k_yes_size, k_no_size) = market.kalshi.load();
    let (p_yes, p_no, p_yes_size, p_no_size) = market.poly.load();

    // Priority order: cross-platform arbs first (more reliable)
    let (yes_price, no_price, yes_size, no_size, arb_type) = if arb_mask & 1 != 0 {
        // Poly YES + Kalshi NO
        (p_yes, k_no, p_yes_size, k_no_size, ArbType::PolyYesKalshiNo)
    } else if arb_mask & 2 != 0 {
        // Kalshi YES + Poly NO
        (k_yes, p_no, k_yes_size, p_no_size, ArbType::KalshiYesPolyNo)
    } else if arb_mask & 4 != 0 {
        // Poly only (both sides)
        (p_yes, p_no, p_yes_size, p_no_size, ArbType::PolyOnly)
    } else if arb_mask & 8 != 0 {
        // Kalshi only (both sides)
        (k_yes, k_no, k_yes_size, k_no_size, ArbType::KalshiOnly)
    } else {
        return;
    };

    let req = FastExecutionRequest {
        market_id,
        yes_price,
        no_price,
        yes_size,
        no_size,
        arb_type,
        detected_ns: clock.now_ns(),
    };

    // send! ~~ 
    let _ = exec_tx.try_send(req);
}