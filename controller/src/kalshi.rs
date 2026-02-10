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

use crate::arb::detect_arb;
use crate::config::{self, KALSHI_WS_URL, KALSHI_API_BASE, KALSHI_API_DELAY_MS};
use crate::execution::NanoClock;
use crate::types::{
    KalshiEventsResponse, KalshiMarketsResponse, KalshiEvent, KalshiMarket,
    GlobalState, ArbOpportunity, PriceCents, SizeCents, fxhash_str,
    MarketPair, OrderbookDepth, PriceLevel, DEPTH_LEVELS,
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

                                            // Check for arbs using detect_arb() (routes to depth or top-of-book based on USE_DEPTH)
                                            // Clone depth data before the if-let to avoid holding the read guard across await
                                            let kalshi_depth = market.kalshi.read().clone();
                                            let poly_depth = market.poly.read().clone();
                                            if let Some(req) = detect_arb(
                                                market_id,
                                                &kalshi_depth,
                                                &poly_depth,
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

                                            // Check for arbs using detect_arb() (routes to depth or top-of-book based on USE_DEPTH)
                                            // Clone depth data before the if-let to avoid holding the read guard across await
                                            let kalshi_depth = market.kalshi.read().clone();
                                            let poly_depth = market.poly.read().clone();
                                            if let Some(req) = detect_arb(
                                                market_id,
                                                &kalshi_depth,
                                                &poly_depth,
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
/// Collects top 3 price levels for depth-aware arbitrage detection.
#[inline]
fn process_kalshi_snapshot(market: &crate::types::AtomicMarketState, body: &KalshiWsMsgBody) {
    let ticker = body.market_ticker.as_deref().unwrap_or("unknown");

    // Debug: log raw orderbook data
    if let Some(yes_bids) = &body.yes {
        let best_yes_bid = yes_bids.iter()
            .filter(|l| l.len() >= 2 && l[1] > 0)
            .max_by_key(|l| l[0]);
        tracing::debug!(
            "[KALSHI-SNAP] {} | YES bids: {:?} | best_yes_bid: {:?}",
            ticker, yes_bids, best_yes_bid
        );
    }
    if let Some(no_bids) = &body.no {
        let best_no_bid = no_bids.iter()
            .filter(|l| l.len() >= 2 && l[1] > 0)
            .max_by_key(|l| l[0]);
        tracing::debug!(
            "[KALSHI-SNAP] {} | NO bids: {:?} | best_no_bid: {:?}",
            ticker, no_bids, best_no_bid
        );
    }

    // Collect top 3 YES bids (highest prices) - these determine NO asks
    // Sort by bid price descending (best bid first), then convert to asks (ascending order)
    let mut no_levels = [PriceLevel::default(); DEPTH_LEVELS];
    if let Some(levels) = body.yes.as_ref() {
        let mut bids: Vec<_> = levels.iter()
            .filter_map(|l| {
                if l.len() >= 2 && l[1] > 0 {
                    Some((l[0], l[1]))  // (price, qty)
                } else {
                    None
                }
            })
            .collect();
        // Sort by price descending (best bid = highest price first)
        bids.sort_by(|a, b| b.0.cmp(&a.0));

        for (i, (bid_price, qty)) in bids.iter().take(DEPTH_LEVELS).enumerate() {
            let ask_price = (100 - bid_price) as PriceCents;  // To buy NO, pay 100 - YES_bid
            let size = (qty * bid_price / 100) as SizeCents;
            no_levels[i] = PriceLevel { price: ask_price, size };
        }
    }

    // Collect top 3 NO bids (highest prices) - these determine YES asks
    // Sort by bid price descending (best bid first), then convert to asks (ascending order)
    let mut yes_levels = [PriceLevel::default(); DEPTH_LEVELS];
    if let Some(levels) = body.no.as_ref() {
        let mut bids: Vec<_> = levels.iter()
            .filter_map(|l| {
                if l.len() >= 2 && l[1] > 0 {
                    Some((l[0], l[1]))  // (price, qty)
                } else {
                    None
                }
            })
            .collect();
        // Sort by price descending (best bid = highest price first)
        bids.sort_by(|a, b| b.0.cmp(&a.0));

        for (i, (bid_price, qty)) in bids.iter().take(DEPTH_LEVELS).enumerate() {
            let ask_price = (100 - bid_price) as PriceCents;  // To buy YES, pay 100 - NO_bid
            let size = (qty * bid_price / 100) as SizeCents;
            yes_levels[i] = PriceLevel { price: ask_price, size };
        }
    }

    // Debug: log computed prices
    tracing::debug!(
        "[KALSHI-SNAP] {} | COMPUTED: yes_ask={}¢ no_ask={}¢ yes_size={}¢ no_size={}¢ (top-of-book)",
        ticker, yes_levels[0].price, no_levels[0].price, yes_levels[0].size, no_levels[0].size
    );

    // Store depth using write lock
    let depth = OrderbookDepth { yes_levels, no_levels };
    *market.kalshi.write() = depth;
    market.inc_kalshi_updates();
}

/// Process Kalshi orderbook delta
/// Note: Deltas update bid levels; we recompute asks from best bids.
/// Collects top 3 price levels for depth-aware arbitrage detection.
/// If a side has no delta, we preserve the current levels for that side.
#[inline]
fn process_kalshi_delta(market: &crate::types::AtomicMarketState, body: &KalshiWsMsgBody) {
    let ticker = body.market_ticker.as_deref().unwrap_or("unknown");

    // Read current depth state
    let current_depth = { *market.kalshi.read() };
    let (current_yes, current_no) = (current_depth.yes_levels[0].price, current_depth.no_levels[0].price);

    // Debug: log delta updates
    if body.yes.is_some() || body.no.is_some() {
        tracing::debug!(
            "[KALSHI-DELTA] {} | YES delta: {:?} | NO delta: {:?} | current: yes={}¢ no={}¢",
            ticker, body.yes, body.no, current_yes, current_no
        );
    }

    // Process YES bid updates (affects NO asks)
    let no_levels = if let Some(levels) = &body.yes {
        let mut bids: Vec<_> = levels.iter()
            .filter_map(|l| {
                if l.len() >= 2 && l[1] > 0 {
                    Some((l[0], l[1]))  // (price, qty)
                } else {
                    None
                }
            })
            .collect();
        // Sort by price descending (best bid = highest price first)
        bids.sort_by(|a, b| b.0.cmp(&a.0));

        let mut no_lvls = [PriceLevel::default(); DEPTH_LEVELS];
        for (i, (bid_price, qty)) in bids.iter().take(DEPTH_LEVELS).enumerate() {
            let ask_price = (100 - bid_price) as PriceCents;
            let size = (qty * bid_price / 100) as SizeCents;
            no_lvls[i] = PriceLevel { price: ask_price, size };
        }
        // If delta has fewer levels than DEPTH_LEVELS, the rest stay at default (0, 0)
        no_lvls
    } else {
        current_depth.no_levels
    };

    // Process NO bid updates (affects YES asks)
    let yes_levels = if let Some(levels) = &body.no {
        let mut bids: Vec<_> = levels.iter()
            .filter_map(|l| {
                if l.len() >= 2 && l[1] > 0 {
                    Some((l[0], l[1]))  // (price, qty)
                } else {
                    None
                }
            })
            .collect();
        // Sort by price descending (best bid = highest price first)
        bids.sort_by(|a, b| b.0.cmp(&a.0));

        let mut yes_lvls = [PriceLevel::default(); DEPTH_LEVELS];
        for (i, (bid_price, qty)) in bids.iter().take(DEPTH_LEVELS).enumerate() {
            let ask_price = (100 - bid_price) as PriceCents;
            let size = (qty * bid_price / 100) as SizeCents;
            yes_lvls[i] = PriceLevel { price: ask_price, size };
        }
        yes_lvls
    } else {
        current_depth.yes_levels
    };

    // Debug: log computed prices after delta
    tracing::debug!(
        "[KALSHI-DELTA] {} | COMPUTED: yes_ask={}¢ no_ask={}¢ (was yes={}¢ no={}¢)",
        ticker, yes_levels[0].price, no_levels[0].price, current_yes, current_no
    );

    // Store depth using write lock
    let depth = OrderbookDepth { yes_levels, no_levels };
    *market.kalshi.write() = depth;
    market.inc_kalshi_updates();
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

// =============================================================================
// Helper functions for orderbook parsing (testable)
// =============================================================================

/// Convert Kalshi bid levels to ask levels.
///
/// Kalshi sends bids for YES/NO. To buy the OPPOSITE side, we pay `100 - bid_price`.
/// For example:
/// - If someone bids 52 cents for YES, we can buy NO at 48 cents (100 - 52).
/// - The size in cents is `qty * bid_price / 100` (Kalshi uses whole contracts).
///
/// # Arguments
/// * `bids` - Array of [price_cents, quantity] pairs
///
/// # Returns
/// Array of `DEPTH_LEVELS` PriceLevel structs, sorted by ask price ascending.
/// Empty levels (beyond available bids) are default (price=0, size=0).
pub fn convert_bids_to_asks(bids: &[Vec<i64>]) -> [PriceLevel; DEPTH_LEVELS] {
    let mut levels = [PriceLevel::default(); DEPTH_LEVELS];

    // Filter valid bids (len >= 2, qty > 0) and collect as (price, qty)
    let mut valid_bids: Vec<(i64, i64)> = bids.iter()
        .filter_map(|l| {
            if l.len() >= 2 && l[1] > 0 {
                Some((l[0], l[1]))
            } else {
                None
            }
        })
        .collect();

    // Sort by price descending (best bid = highest price first)
    // This gives us the best asks (lowest prices) first after conversion
    valid_bids.sort_by(|a, b| b.0.cmp(&a.0));

    // Convert top bids to asks
    for (i, (bid_price, qty)) in valid_bids.iter().take(DEPTH_LEVELS).enumerate() {
        let ask_price = (100 - bid_price) as PriceCents;  // To buy opposite side
        let size = (qty * bid_price / 100) as SizeCents;  // Convert to cents
        levels[i] = PriceLevel { price: ask_price, size };
    }

    levels
}

#[cfg(test)]
mod orderbook_depth_tests {
    use super::*;

    // =========================================================================
    // Tests for Kalshi bid-to-ask conversion logic
    // =========================================================================

    #[test]
    fn test_exactly_three_bids() {
        // Bids at 52, 50, 48 cents with quantities 100, 200, 150
        let bids = vec![
            vec![52, 100],
            vec![50, 200],
            vec![48, 150],
        ];

        let asks = convert_bids_to_asks(&bids);

        // Best bid (52) → best ask (48 = 100 - 52)
        // ask_price = 100 - bid_price
        // size = qty * bid_price / 100

        // Level 0: bid 52 → ask 48, size = 100 * 52 / 100 = 52
        assert_eq!(asks[0].price, 48, "Level 0 ask price should be 100 - 52 = 48");
        assert_eq!(asks[0].size, 52, "Level 0 size should be 100 * 52 / 100 = 52");

        // Level 1: bid 50 → ask 50, size = 200 * 50 / 100 = 100
        assert_eq!(asks[1].price, 50, "Level 1 ask price should be 100 - 50 = 50");
        assert_eq!(asks[1].size, 100, "Level 1 size should be 200 * 50 / 100 = 100");

        // Level 2: bid 48 → ask 52, size = 150 * 48 / 100 = 72
        assert_eq!(asks[2].price, 52, "Level 2 ask price should be 100 - 48 = 52");
        assert_eq!(asks[2].size, 72, "Level 2 size should be 150 * 48 / 100 = 72");
    }

    #[test]
    fn test_fewer_than_three_bids() {
        // Only one bid at 50 cents
        let bids = vec![vec![50, 100]];

        let asks = convert_bids_to_asks(&bids);

        // Level 0 should be populated
        assert_eq!(asks[0].price, 50, "Level 0 ask price should be 100 - 50 = 50");
        assert_eq!(asks[0].size, 50, "Level 0 size should be 100 * 50 / 100 = 50");

        // Levels 1 and 2 should be zero (default)
        assert_eq!(asks[1].price, 0, "Level 1 should be empty (price = 0)");
        assert_eq!(asks[1].size, 0, "Level 1 should be empty (size = 0)");
        assert_eq!(asks[2].price, 0, "Level 2 should be empty (price = 0)");
        assert_eq!(asks[2].size, 0, "Level 2 should be empty (size = 0)");
    }

    #[test]
    fn test_more_than_three_bids_takes_top_three() {
        // 5 bids, should take top 3 (highest prices = best asks)
        let bids = vec![
            vec![30, 100],  // Lowest bid, worst ask (70)
            vec![60, 200],  // Third highest bid, should be included (40)
            vec![40, 150],  //
            vec![80, 300],  // Highest bid, best ask (20)
            vec![70, 250],  // Second highest bid (30)
        ];

        let asks = convert_bids_to_asks(&bids);

        // Should take bids at 80, 70, 60 (highest first)
        // Ask prices: 20, 30, 40 (ascending from best bid descending)

        // Level 0: bid 80 → ask 20, size = 300 * 80 / 100 = 240
        assert_eq!(asks[0].price, 20, "Best ask should be 100 - 80 = 20");
        assert_eq!(asks[0].size, 240, "Size should be 300 * 80 / 100 = 240");

        // Level 1: bid 70 → ask 30, size = 250 * 70 / 100 = 175
        assert_eq!(asks[1].price, 30, "Second ask should be 100 - 70 = 30");
        assert_eq!(asks[1].size, 175, "Size should be 250 * 70 / 100 = 175");

        // Level 2: bid 60 → ask 40, size = 200 * 60 / 100 = 120
        assert_eq!(asks[2].price, 40, "Third ask should be 100 - 60 = 40");
        assert_eq!(asks[2].size, 120, "Size should be 200 * 60 / 100 = 120");
    }

    #[test]
    fn test_empty_bids() {
        let bids: Vec<Vec<i64>> = vec![];
        let asks = convert_bids_to_asks(&bids);

        // All levels should be zero
        for (i, level) in asks.iter().enumerate() {
            assert_eq!(level.price, 0, "Level {} price should be 0", i);
            assert_eq!(level.size, 0, "Level {} size should be 0", i);
        }
    }

    #[test]
    fn test_bids_with_zero_quantity_filtered() {
        // Zero quantity bids should be filtered out
        let bids = vec![
            vec![50, 0],   // Invalid: zero qty
            vec![45, 100], // Valid
            vec![40, 0],   // Invalid: zero qty
        ];

        let asks = convert_bids_to_asks(&bids);

        // Only the 45 cent bid should be used
        assert_eq!(asks[0].price, 55, "Should use bid at 45, ask = 55");
        assert_eq!(asks[0].size, 45, "Size = 100 * 45 / 100 = 45");

        // Rest should be zero
        assert_eq!(asks[1].price, 0);
        assert_eq!(asks[2].price, 0);
    }

    #[test]
    fn test_bids_with_malformed_data_filtered() {
        // Bids with less than 2 elements should be filtered
        let bids = vec![
            vec![50],        // Invalid: only 1 element
            vec![45, 100],   // Valid
            vec![],          // Invalid: empty
        ];

        let asks = convert_bids_to_asks(&bids);

        assert_eq!(asks[0].price, 55, "Should use only valid bid");
        assert_eq!(asks[0].size, 45);
        assert_eq!(asks[1].price, 0);
        assert_eq!(asks[2].price, 0);
    }

    #[test]
    fn test_bid_to_ask_price_conversion_edge_cases() {
        // Test edge case prices

        // Bid at 1 cent → ask at 99 cents
        let bids_low = vec![vec![1, 100]];
        let asks_low = convert_bids_to_asks(&bids_low);
        assert_eq!(asks_low[0].price, 99, "Bid 1 → Ask 99");
        assert_eq!(asks_low[0].size, 1, "Size = 100 * 1 / 100 = 1");

        // Bid at 99 cents → ask at 1 cent
        let bids_high = vec![vec![99, 100]];
        let asks_high = convert_bids_to_asks(&bids_high);
        assert_eq!(asks_high[0].price, 1, "Bid 99 → Ask 1");
        assert_eq!(asks_high[0].size, 99, "Size = 100 * 99 / 100 = 99");

        // Bid at 50 cents → ask at 50 cents (symmetric case)
        let bids_mid = vec![vec![50, 100]];
        let asks_mid = convert_bids_to_asks(&bids_mid);
        assert_eq!(asks_mid[0].price, 50, "Bid 50 → Ask 50 (symmetric)");
        assert_eq!(asks_mid[0].size, 50, "Size = 100 * 50 / 100 = 50");
    }

    #[test]
    fn test_size_calculation_precision() {
        // Test that size calculation handles rounding correctly
        // size = qty * bid_price / 100

        // Large quantity: 1000 contracts at 75 cents → 750 cents size
        let bids = vec![vec![75, 1000]];
        let asks = convert_bids_to_asks(&bids);
        assert_eq!(asks[0].size, 750, "1000 * 75 / 100 = 750");

        // Small quantity: 10 contracts at 33 cents → 3 cents (integer division)
        let bids2 = vec![vec![33, 10]];
        let asks2 = convert_bids_to_asks(&bids2);
        assert_eq!(asks2[0].size, 3, "10 * 33 / 100 = 3 (integer division)");

        // Edge: 1 contract at 50 cents → 0 cents (too small)
        let bids3 = vec![vec![50, 1]];
        let asks3 = convert_bids_to_asks(&bids3);
        assert_eq!(asks3[0].size, 0, "1 * 50 / 100 = 0 (integer division truncates)");
    }

    #[test]
    fn test_bids_already_sorted_descending() {
        // Bids already in descending order
        let bids = vec![
            vec![90, 100],
            vec![80, 200],
            vec![70, 300],
        ];

        let asks = convert_bids_to_asks(&bids);

        // Should preserve order: best bid (90) → best ask (10)
        assert_eq!(asks[0].price, 10);
        assert_eq!(asks[1].price, 20);
        assert_eq!(asks[2].price, 30);
    }

    #[test]
    fn test_bids_in_ascending_order() {
        // Bids in ascending order (needs sorting)
        let bids = vec![
            vec![70, 300],
            vec![80, 200],
            vec![90, 100],
        ];

        let asks = convert_bids_to_asks(&bids);

        // After sorting, should get same result as descending input
        assert_eq!(asks[0].price, 10);
        assert_eq!(asks[1].price, 20);
        assert_eq!(asks[2].price, 30);
    }

    #[test]
    fn test_two_bids_leaves_third_level_empty() {
        let bids = vec![
            vec![60, 100],
            vec![55, 200],
        ];

        let asks = convert_bids_to_asks(&bids);

        // First two levels populated
        assert_eq!(asks[0].price, 40, "Bid 60 → Ask 40");
        assert_eq!(asks[1].price, 45, "Bid 55 → Ask 45");

        // Third level empty
        assert_eq!(asks[2].price, 0);
        assert_eq!(asks[2].size, 0);
    }
}