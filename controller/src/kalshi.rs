//! Kalshi platform integration client.
//!
//! This module provides REST API and WebSocket clients for interacting with
//! the Kalshi prediction market platform, including order execution and
//! real-time price feed management.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::{SinkExt, StreamExt};
use pkcs1::DecodeRsaPrivateKey;
use rsa::{
    pss::SigningKey,
    sha2::Sha256,
    signature::{RandomizedSigner, SignatureEncoding},
    RsaPrivateKey,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::{http::Request, Message}};
use tracing::{debug, error, info, warn};

use crate::config::{self, KALSHI_WS_URL, KALSHI_API_BASE, KALSHI_API_DELAY_MS};
use crate::execution::NanoClock;
use crate::types::{
    KalshiEventsResponse, KalshiMarketsResponse, KalshiEvent, KalshiMarket,
    GlobalState, ArbOpportunity, PriceCents, fxhash_str,
    MarketPair,
};
use crate::debug_socket::{DebugBroadcaster, build_market_update_json};

// === Order Types ===

use std::borrow::Cow;
use std::fmt::Write;
use arrayvec::ArrayString;

#[derive(Debug, Clone, Serialize)]
pub struct KalshiOrderRequest<'a> {
    pub ticker: Cow<'a, str>,
    pub action: &'static str,
    pub side: &'static str,
    #[serde(rename = "type")]
    pub order_type: &'static str,
    pub count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_price: Option<i64>,
    pub client_order_id: Cow<'a, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<&'static str>,
}

impl<'a> KalshiOrderRequest<'a> {
    /// Create an IOC (immediate-or-cancel) buy order
    pub fn ioc_buy(ticker: Cow<'a, str>, side: &'static str, price_cents: i64, count: i64, client_order_id: Cow<'a, str>) -> Self {
        let (yes_price, no_price) = if side == "yes" {
            (Some(price_cents), None)
        } else {
            (None, Some(price_cents))
        };

        Self {
            ticker,
            action: "buy",
            side,
            order_type: "limit",
            count,
            yes_price,
            no_price,
            client_order_id,
            expiration_ts: None,
            time_in_force: Some("immediate_or_cancel"),
        }
    }

    /// Create an IOC (immediate-or-cancel) sell order
    pub fn ioc_sell(ticker: Cow<'a, str>, side: &'static str, price_cents: i64, count: i64, client_order_id: Cow<'a, str>) -> Self {
        let (yes_price, no_price) = if side == "yes" {
            (Some(price_cents), None)
        } else {
            (None, Some(price_cents))
        };

        Self {
            ticker,
            action: "sell",
            side,
            order_type: "limit",
            count,
            yes_price,
            no_price,
            client_order_id,
            expiration_ts: None,
            time_in_force: Some("immediate_or_cancel"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrderResponse {
    pub order: KalshiOrderDetails,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrderDetails {
    pub order_id: String,
    pub ticker: String,
    pub status: String,        // "resting", "canceled", "executed", "pending"
    #[serde(default)]
    pub remaining_count: Option<i64>,
    #[serde(default)]
    pub queue_position: Option<i64>,
    pub action: String,
    pub side: String,
    /// Total filled count (primary field from API)
    #[serde(default)]
    pub fill_count: Option<i64>,
    #[serde(rename = "type")]
    pub order_type: String,
    pub yes_price: Option<i64>,
    pub no_price: Option<i64>,
    pub created_time: Option<String>,
    #[serde(default)]
    pub taker_fill_count: Option<i64>,
    #[serde(default)]
    pub maker_fill_count: Option<i64>,
    #[serde(default)]
    pub place_count: Option<i64>,
    #[serde(default)]
    pub taker_fill_cost: Option<i64>,
    #[serde(default)]
    pub maker_fill_cost: Option<i64>,
    #[serde(default)]
    pub taker_fees: Option<i64>,
    #[serde(default)]
    pub maker_fees: Option<i64>,
}

/// Response from GET /portfolio/balance
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiBalance {
    /// Available cash balance in cents
    pub balance: i64,
    /// Total value of open positions in cents
    pub portfolio_value: i64,
    /// Last update timestamp (milliseconds since epoch)
    #[serde(default)]
    pub updated_ts: Option<i64>,
}

#[allow(dead_code)]
impl KalshiOrderDetails {
    /// Total filled contracts
    pub fn filled_count(&self) -> i64 {
        // Use fill_count if available (primary field), otherwise sum taker+maker
        self.fill_count.unwrap_or_else(|| {
            self.taker_fill_count.unwrap_or(0) + self.maker_fill_count.unwrap_or(0)
        })
    }

    /// Check if order was fully filled
    pub fn is_filled(&self) -> bool {
        self.status == "executed" || self.remaining_count == Some(0)
    }

    /// Check if order was partially filled
    pub fn is_partial(&self) -> bool {
        self.filled_count() > 0 && !self.is_filled()
    }

    /// Total fees paid (in cents)
    pub fn total_fees(&self) -> i64 {
        self.taker_fees.unwrap_or(0) + self.maker_fees.unwrap_or(0)
    }
}

// === Kalshi Auth Config ===

pub struct KalshiConfig {
    pub api_key_id: String,
    pub private_key: RsaPrivateKey,
}

impl KalshiConfig {
    pub fn from_env() -> Result<Self> {
        crate::paths::load_dotenv();
        let api_key_id = std::env::var("KALSHI_API_KEY_ID").context("KALSHI_API_KEY_ID not set")?;
        // Support both KALSHI_PRIVATE_KEY_PATH and KALSHI_PRIVATE_KEY_FILE for compatibility
        let key_path = std::env::var("KALSHI_PRIVATE_KEY_PATH")
            .or_else(|_| std::env::var("KALSHI_PRIVATE_KEY_FILE"))
            .unwrap_or_else(|_| "kalshi_private_key.txt".to_string());
        let resolved_key_path = crate::paths::resolve_user_path(&key_path);
        let private_key_pem = std::fs::read_to_string(&resolved_key_path)
            .with_context(|| {
                format!(
                    "Failed to read private key from {} (resolved to {})",
                    key_path,
                    resolved_key_path.display()
                )
            })?
            .trim()
            .to_owned();
        let private_key = RsaPrivateKey::from_pkcs1_pem(&private_key_pem)
            .context("Failed to parse private key PEM")?;
        Ok(Self { api_key_id, private_key })
    }

    pub fn sign(&self, message: &str) -> Result<String> {
        let signing_key = SigningKey::<Sha256>::new(self.private_key.clone());
        let signature = signing_key.sign_with_rng(&mut rand::thread_rng(), message.as_bytes());
        let sig_b64 = BASE64.encode(signature.to_bytes());
        Ok(sig_b64)
    }
}

// === Kalshi REST API Client ===

/// Timeout for order requests (shorter than general API timeout)
const ORDER_TIMEOUT: Duration = Duration::from_secs(5);

use std::sync::atomic::{AtomicU32, Ordering};

/// Global order counter for unique client_order_id generation
static ORDER_COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct KalshiApiClient {
    http: reqwest_middleware::ClientWithMiddleware,
    pub config: KalshiConfig,
    base_url: String,
}

impl KalshiApiClient {
    pub fn new(config: KalshiConfig) -> Self {
        Self::new_with_base_url(config, KALSHI_API_BASE)
    }

    /// Create a new client with a custom base URL (for testing with mock servers).
    pub fn new_with_base_url(config: KalshiConfig, base_url: &str) -> Self {
        Self {
            http: trading::capture::build_kalshi_client(Duration::from_secs(10)),
            config,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    #[inline]
    fn next_order_id() -> ArrayString<24> {
        let counter = ORDER_COUNTER.fetch_add(1, Ordering::Relaxed);
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mut buf = ArrayString::<24>::new();
        let _ = write!(&mut buf, "a{}{}", ts, counter);
        buf
    }
    
    /// Generic authenticated GET request with retry on rate limit
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut retries = 0;
        const MAX_RETRIES: u32 = 5;

        loop {
            let url = format!("{}{}", self.base_url, path);
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            // Kalshi signature uses FULL path including /trade-api/v2 prefix
            let full_path = format!("/trade-api/v2{}", path);
            let signature = self.config.sign(&format!("{}GET{}", timestamp_ms, full_path))?;
            
            let resp = self.http
                .get(&url)
                .header("KALSHI-ACCESS-KEY", &self.config.api_key_id)
                .header("KALSHI-ACCESS-SIGNATURE", &signature)
                .header("KALSHI-ACCESS-TIMESTAMP", timestamp_ms.to_string())
                .send()
                .await?;
            
            let status = resp.status();
            
            // Handle rate limit with exponential backoff
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                retries += 1;
                if retries > MAX_RETRIES {
                    anyhow::bail!("Kalshi API rate limited after {} retries", MAX_RETRIES);
                }
                let backoff_ms = 2000 * (1 << retries); // 4s, 8s, 16s, 32s, 64s
                debug!("[KALSHI] Rate limited, backing off {}ms (retry {}/{})", 
                       backoff_ms, retries, MAX_RETRIES);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                continue;
            }
            
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Kalshi API error {}: {}", status, body);
            }
            
            let data: T = resp.json().await?;
            tokio::time::sleep(Duration::from_millis(KALSHI_API_DELAY_MS)).await;
            return Ok(data);
        }
    }
    
    pub async fn get_events(&self, series_ticker: &str, limit: u32) -> Result<Vec<KalshiEvent>> {
        let path = format!("/events?series_ticker={}&limit={}&status=open", series_ticker, limit);
        let resp: KalshiEventsResponse = self.get(&path).await?;
        Ok(resp.events)
    }

    /// Get markets for a given series ticker (fast path - single call instead of N+1).
    ///
    /// Note: Kalshi's API accepts `status=open` to return actively tradable markets (often `status: "active"`).
    pub async fn get_markets_for_series(&self, series_ticker: &str, limit: u32) -> Result<Vec<KalshiMarket>> {
        let path = format!("/markets?series_ticker={}&status=open&limit={}", series_ticker, limit);
        let resp: KalshiMarketsResponse = self.get(&path).await?;
        Ok(resp.markets)
    }

    /// Get markets created since a given timestamp for a series
    pub async fn get_markets_since(&self, series_ticker: &str, since_ts: u64) -> Result<Vec<KalshiMarket>> {
        let path = format!(
            "/markets?series_ticker={}&min_created_ts={}&status=open&limit=100",
            series_ticker, since_ts
        );
        let resp: KalshiMarketsResponse = self.get(&path).await?;
        Ok(resp.markets)
    }

    /// Get markets for a given event ticker
    pub async fn get_markets_for_event(&self, event_ticker: &str) -> Result<Vec<KalshiMarket>> {
        let path = format!("/markets?event_ticker={}&status=open&limit=100", event_ticker);
        let resp: KalshiMarketsResponse = self.get(&path).await?;
        Ok(resp.markets)
    }

    /// Get portfolio balance
    pub async fn get_balance(&self) -> Result<KalshiBalance> {
        self.get("/portfolio/balance").await
    }

    /// Generic authenticated POST request
    async fn post<T: serde::de::DeserializeOwned, B: Serialize>(&self, path: &str, body: &B) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // Kalshi signature uses FULL path including /trade-api/v2 prefix
        let full_path = format!("/trade-api/v2{}", path);
        let msg = format!("{}POST{}", timestamp_ms, full_path);
        let signature = self.config.sign(&msg)?;

        // Serialize body manually (reqwest-middleware doesn't expose .json())
        let body_bytes = serde_json::to_vec(body)?;

        let resp = self.http
            .post(&url)
            .header("KALSHI-ACCESS-KEY", &self.config.api_key_id)
            .header("KALSHI-ACCESS-SIGNATURE", &signature)
            .header("KALSHI-ACCESS-TIMESTAMP", timestamp_ms.to_string())
            .header("Content-Type", "application/json")
            .timeout(ORDER_TIMEOUT)
            .body(body_bytes)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Kalshi API error {}: {}", status, body);
        }

        let data: T = resp.json().await?;
        Ok(data)
    }
    
    /// Create an order on Kalshi
    pub async fn create_order(&self, order: &KalshiOrderRequest<'_>) -> Result<KalshiOrderResponse> {
        let path = "/portfolio/orders";
        self.post(path, order).await
    }
    
    /// Create an IOC buy order (convenience method)
    pub async fn buy_ioc(
        &self,
        ticker: &str,
        side: &str,  // "yes" or "no"
        price_cents: i64,
        count: i64,
    ) -> Result<KalshiOrderResponse> {
        debug_assert!(!ticker.is_empty(), "ticker must not be empty");
        debug_assert!((1..=99).contains(&price_cents), "price must be 1-99");
        debug_assert!(count >= 1, "count must be >= 1");

        let side_static: &'static str = if side == "yes" { "yes" } else { "no" };
        let order_id = Self::next_order_id();
        let order = KalshiOrderRequest::ioc_buy(
            Cow::Borrowed(ticker),
            side_static,
            price_cents,
            count,
            Cow::Borrowed(&order_id)
        );
        debug!("[KALSHI] IOC {} {} @{}¢ x{}", side, ticker, price_cents, count);

        let resp = self.create_order(&order).await?;
        let filled = resp.order.filled_count();
        debug!("[KALSHI] {} filled={}", resp.order.status, filled);

        // Warn if IOC order was canceled with no fills (likely no liquidity at requested price)
        if resp.order.status == "canceled" && filled == 0 {
            warn!(
                "[KALSHI] IOC BUY {} {} @{}¢ x{} was CANCELED with 0 fills (no liquidity at price?)",
                side, ticker, price_cents, count
            );
        }

        Ok(resp)
    }

    pub async fn sell_ioc(
        &self,
        ticker: &str,
        side: &str,
        price_cents: i64,
        count: i64,
    ) -> Result<KalshiOrderResponse> {
        debug_assert!(!ticker.is_empty(), "ticker must not be empty");
        debug_assert!((1..=99).contains(&price_cents), "price must be 1-99");
        debug_assert!(count >= 1, "count must be >= 1");

        let side_static: &'static str = if side == "yes" { "yes" } else { "no" };
        let order_id = Self::next_order_id();
        let order = KalshiOrderRequest::ioc_sell(
            Cow::Borrowed(ticker),
            side_static,
            price_cents,
            count,
            Cow::Borrowed(&order_id)
        );
        debug!("[KALSHI] SELL {} {} @{}¢ x{}", side, ticker, price_cents, count);

        let resp = self.create_order(&order).await?;
        let filled = resp.order.filled_count();
        debug!("[KALSHI] {} filled={}", resp.order.status, filled);

        // Warn if IOC order was canceled with no fills (likely no liquidity at price)
        if resp.order.status == "canceled" && filled == 0 {
            warn!(
                "[KALSHI] IOC SELL {} {} @{}¢ x{} was CANCELED with 0 fills (no liquidity at price?)",
                side, ticker, price_cents, count
            );
        }

        Ok(resp)
    }
}

// === WebSocket Message Types ===

#[derive(Deserialize, Debug)]
pub struct KalshiWsMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub msg: Option<KalshiWsMsgBody>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct KalshiWsMsgBody {
    pub market_ticker: Option<String>,
    // Snapshot fields - arrays of [price_cents, quantity]
    pub yes: Option<Vec<Vec<i64>>>,
    pub no: Option<Vec<Vec<i64>>>,
    // Delta fields
    pub price: Option<i64>,
    pub delta: Option<i64>,
    pub side: Option<String>,
}

#[derive(Serialize)]
struct SubscribeCmd {
    id: i32,
    cmd: &'static str,
    params: SubscribeParams,
}

#[derive(Serialize)]
struct SubscribeParams {
    channels: Vec<&'static str>,
    market_tickers: Vec<String>,
}

// =============================================================================
// WebSocket Runner
// =============================================================================

/// WebSocket runner
pub async fn run_ws(
    config: &KalshiConfig,
    state: Arc<GlobalState>,
    exec_tx: mpsc::Sender<ArbOpportunity>,
    confirm_tx: mpsc::Sender<(ArbOpportunity, Arc<MarketPair>)>,
    _threshold_cents: PriceCents,
    mut shutdown_rx: watch::Receiver<bool>,
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

    let tickers: Vec<String> = state.markets.iter()
        .take(state.market_count())
        .filter_map(|m| m.pair().map(|p| p.kalshi_market_ticker.to_string()))
        .collect();

    let tui_active = tui_state.read().await.active;
    if tickers.is_empty() {
        log_info("[KALSHI] No markets to monitor", tui_active);
        tokio::time::sleep(Duration::from_secs(u64::MAX)).await;
        return Ok(());
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string();

    let signature = config.sign(&format!("{}GET/trade-api/ws/v2", timestamp))?;

    let request = Request::builder()
        .uri(KALSHI_WS_URL)
        .header("KALSHI-ACCESS-KEY", &config.api_key_id)
        .header("KALSHI-ACCESS-SIGNATURE", &signature)
        .header("KALSHI-ACCESS-TIMESTAMP", &timestamp)
        .header("Host", "api.elections.kalshi.com")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
        .body(())?;

    let (ws_stream, _) = connect_async(request).await.context("Failed to connect to Kalshi")?;
    let tui_active = tui_state.read().await.active;
    log_info("[KALSHI] Connected", tui_active);

    let (mut write, mut read) = ws_stream.split();

    // Subscribe to all tickers
    let subscribe_msg = SubscribeCmd {
        id: 1,
        cmd: "subscribe",
        params: SubscribeParams {
            channels: vec!["orderbook_delta"],
            market_tickers: tickers.clone(),
        },
    };

    write.send(Message::Text(serde_json::to_string(&subscribe_msg)?)).await?;
    let tui_active = tui_state.read().await.active;
    log_info(&format!("[KALSHI] Subscribed to {} markets", tickers.len()), tui_active);

    loop {
        tokio::select! {
            biased;

            // Check shutdown signal first
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let tui_active = tui_state.read().await.active;
                    log_info("[KALSHI] Shutdown signal received, disconnecting...", tui_active);
                    break;
                }
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<KalshiWsMessage>(&text) {
                            Ok(kalshi_msg) => {
                                let ticker = kalshi_msg.msg.as_ref()
                                    .and_then(|m| m.market_ticker.as_ref());

                                let Some(ticker) = ticker else { continue };
                                let ticker_hash = fxhash_str(ticker);

                                let Some(market_id) = state.kalshi_to_id.read().get(&ticker_hash).copied() else { continue };
                                let market = &state.markets[market_id as usize];

                                match kalshi_msg.msg_type.as_str() {
                                    "orderbook_snapshot" => {
                                        if let Some(body) = &kalshi_msg.msg {
                                            let now_ms = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_millis() as u64;
                                            process_kalshi_snapshot(market, body);
                                            market.mark_kalshi_update_unix_ms(now_ms);
                                            if let Some(ref dbg) = debug {
                                                if let Some(json) = build_market_update_json(&state, market_id) {
                                                    dbg.send_json(json);
                                                }
                                            }

                                            // Check for arbs using ArbOpportunity::detect()
                                            if let Some(req) = ArbOpportunity::detect(
                                                market_id,
                                                market.kalshi.load(),
                                                market.poly.load(),
                                                state.arb_config(),
                                                clock.now_ns(),
                                            ) {
                                                route_arb_to_channel(&state, market_id, req, &exec_tx, &confirm_tx).await;
                                            }
                                        }
                                    }
                                    "orderbook_delta" => {
                                        if let Some(body) = &kalshi_msg.msg {
                                            let now_ms = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_millis() as u64;
                                            process_kalshi_delta(market, body);
                                            market.mark_kalshi_update_unix_ms(now_ms);
                                            if let Some(ref dbg) = debug {
                                                if let Some(json) = build_market_update_json(&state, market_id) {
                                                    dbg.send_json(json);
                                                }
                                            }

                                            // Check for arbs using ArbOpportunity::detect()
                                            if let Some(req) = ArbOpportunity::detect(
                                                market_id,
                                                market.kalshi.load(),
                                                market.poly.load(),
                                                state.arb_config(),
                                                clock.now_ns(),
                                            ) {
                                                route_arb_to_channel(&state, market_id, req, &exec_tx, &confirm_tx).await;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                // Log at trace level - unknown message types are normal
                                tracing::trace!("[KALSHI] WS parse error: {} (msg: {}...)", e, &text[..text.len().min(100)]);
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if let Err(e) = write.send(Message::Pong(data)).await {
                            let tui_active = tui_state.read().await.active;
                            log_warn(&format!("[KALSHI] Failed to send pong: {} (connection may be degraded)", e), tui_active);
                        }
                    }
                    Some(Err(e)) => {
                        let tui_active = tui_state.read().await.active;
                        log_error(&format!("[KALSHI] WebSocket error: {}", e), tui_active);
                        break;
                    }
                    None => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Process Kalshi orderbook snapshot
/// Note: Kalshi sends BIDS - to buy YES you pay (100 - best_NO_bid), to buy NO you pay (100 - best_YES_bid)
#[inline]
fn process_kalshi_snapshot(market: &crate::types::AtomicMarketState, body: &KalshiWsMsgBody) {
    let ticker = body.market_ticker.as_deref().unwrap_or("unknown");

    // Populate KalshiBook from snapshot arrays
    let mut book = market.kalshi_book.lock();

    if let Some(levels) = &body.yes {
        tracing::debug!(
            "[KALSHI-SNAP] {} | YES bids: {:?}",
            ticker, levels
        );
        book.set_yes_bids(levels);
    } else {
        book.set_yes_bids(&[]);
    }

    if let Some(levels) = &body.no {
        tracing::debug!(
            "[KALSHI-SNAP] {} | NO bids: {:?}",
            ticker, levels
        );
        book.set_no_bids(levels);
    } else {
        book.set_no_bids(&[]);
    }

    // Derive AtomicOrderbook from book state
    let (no_ask, no_size) = book.derive_no_side();
    let (yes_ask, yes_size) = book.derive_yes_side();
    drop(book);

    tracing::debug!(
        "[KALSHI-SNAP] {} | COMPUTED: yes_ask={}¢ no_ask={}¢ yes_size={}¢ no_size={}¢",
        ticker, yes_ask, no_ask, yes_size, no_size
    );

    market.kalshi.store(yes_ask, no_ask, yes_size, no_size);
    market.inc_kalshi_updates();
}

/// Process Kalshi orderbook delta
/// Note: Deltas update bid levels; we recompute asks from best bids
#[inline]
fn process_kalshi_delta(market: &crate::types::AtomicMarketState, body: &KalshiWsMsgBody) {
    let ticker = body.market_ticker.as_deref().unwrap_or("unknown");

    // Delta messages use price/delta/side fields (NOT yes/no arrays)
    let (price, delta, side) = match (&body.price, &body.delta, &body.side) {
        (Some(p), Some(d), Some(s)) => (*p, *d, s.as_str()),
        _ => {
            tracing::warn!(
                "[KALSHI-DELTA] {} | Malformed delta: missing price/delta/side fields",
                ticker
            );
            return;
        }
    };

    // Validate price range (Kalshi prediction market prices are 1-99 cents)
    if price < 1 || price > 99 {
        tracing::warn!(
            "[KALSHI-DELTA] {} | Out-of-range price: {} (expected 1-99), dropping delta",
            ticker, price
        );
        return;
    }

    tracing::debug!(
        "[KALSHI-DELTA] {} | side={} price={} delta={}",
        ticker, side, price, delta
    );

    let mut book = market.kalshi_book.lock();
    book.apply_delta(side, price, delta);

    match side {
        "yes" => {
            // YES bid changed → recompute NO ask
            let (no_ask, no_size) = book.derive_no_side();
            drop(book);
            market.kalshi.update_no(no_ask, no_size);
            market.inc_kalshi_updates();
        }
        "no" => {
            // NO bid changed → recompute YES ask
            let (yes_ask, yes_size) = book.derive_yes_side();
            drop(book);
            market.kalshi.update_yes(yes_ask, yes_size);
            market.inc_kalshi_updates();
        }
        _ => {
            drop(book);
            tracing::warn!(
                "[KALSHI-DELTA] {} | Unknown side: {}",
                ticker, side
            );
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
                    "[KALSHI] Arb request dropped for market {}: {} (channel backpressure)",
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
                "[KALSHI] Confirm request dropped for market {}: {} (channel backpressure)",
                market_id, e
            );
        }
    } else {
        if let Err(e) = exec_tx.try_send(req) {
            tracing::warn!(
                "[KALSHI] Arb request dropped for market {}: {} (channel backpressure)",
                market_id, e
            );
        }
    }
}