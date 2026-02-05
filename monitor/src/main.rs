use std::{
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::Query,
    extract::State,
    response::{
        Html,
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::get,
    Json,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::wrappers::WatchStream;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

#[derive(Clone)]
struct AppState {
    positions_tx: watch::Sender<String>,
    markets_tx: broadcast::Sender<String>,
    external: Arc<ExternalState>,
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const POSITIONS_HTML: &str = include_str!("../static/positions.html");

struct ExternalState {
    ttl: Duration,
    cache: tokio::sync::RwLock<Option<CachedExternal>>,
    kalshi: Option<trading::kalshi::KalshiApiClient>,
    http: reqwest::Client,
    poly_user: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedExternal {
    fetched_at: Instant,
    snapshot: ExternalSnapshot,
}

#[derive(Debug, Clone, Serialize)]
struct ExternalSnapshot {
    snapshot_time: String,
    kalshi: ExternalKalshiBlock,
    polymarket: ExternalPolyBlock,
    #[serde(skip_serializing_if = "ExternalErrors::is_empty")]
    errors: ExternalErrors,
}

#[derive(Debug, Clone, Serialize)]
struct ExternalKalshiBlock {
    #[serde(default)]
    positions: Vec<ExternalKalshiPosition>,
}

#[derive(Debug, Clone, Serialize)]
struct ExternalKalshiPosition {
    ticker: String,
    net_position: i64,
    yes_contracts: i64,
    no_contracts: i64,
    total_traded: i64,
    market_exposure_cents: i64,
    realized_pnl_cents: i64,
    fees_paid_cents: i64,
}

#[derive(Debug, Clone, Serialize)]
struct ExternalPolyBlock {
    #[serde(default)]
    positions: Vec<PolyDataPosition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PolyDataPosition {
    #[serde(rename = "proxyWallet")]
    proxy_wallet: Option<String>,
    asset: String,
    #[serde(rename = "conditionId")]
    condition_id: String,
    size: f64,
    #[serde(rename = "avgPrice")]
    avg_price: Option<f64>,
    #[serde(rename = "initialValue")]
    initial_value: Option<f64>,
    #[serde(rename = "currentValue")]
    current_value: Option<f64>,
    #[serde(rename = "cashPnl")]
    cash_pnl: Option<f64>,
    #[serde(rename = "percentPnl")]
    percent_pnl: Option<f64>,
    slug: Option<String>,
    outcome: Option<String>,
    #[serde(rename = "eventSlug")]
    event_slug: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    #[serde(rename = "negativeRisk")]
    negative_risk: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ExternalErrors {
    #[serde(skip_serializing_if = "Option::is_none")]
    kalshi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    polymarket: Option<String>,
}

impl ExternalErrors {
    fn is_empty(&self) -> bool {
        self.kalshi.is_none() && self.polymarket.is_none()
    }
}

#[derive(Debug, Serialize)]
struct OverviewResponse {
    snapshot_time: String,
    internal: serde_json::Value,
    external: ExternalSnapshot,
    reconciliation: Reconciliation,
}

#[derive(Debug, Default, Serialize)]
struct Reconciliation {
    snapshot_time: String,
    summary: ReconcileSummary,
    #[serde(default)]
    by_market_id: std::collections::BTreeMap<String, ReconcileEntry>,
}

#[derive(Debug, Default, Serialize)]
struct ReconcileSummary {
    match_count: u64,
    mismatch_count: u64,
    unknown_mapping_count: u64,
    api_error_count: u64,
}

#[derive(Debug, Serialize)]
struct ReconcileEntry {
    market_id: String,
    kalshi_ticker: Option<String>,
    internal_kalshi_yes: f64,
    internal_kalshi_no: f64,
    external_kalshi_yes: Option<i64>,
    external_kalshi_no: Option<i64>,
    kalshi_diff_yes: Option<f64>,
    kalshi_diff_no: Option<f64>,
    status: String, // match|mismatch|unknown_mapping|api_error
    reason: Option<String>,
}

fn parse_u16_env(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(default)
}

fn positions_path_from_env() -> PathBuf {
    if let Ok(p) = std::env::var("POSITIONS_PATH") {
        return PathBuf::from(p);
    }
    PathBuf::from("positions.json")
}

fn controller_debug_ws_from_env() -> String {
    std::env::var("CONTROLLER_DEBUG_WS").unwrap_or_else(|_| "ws://127.0.0.1:9105".to_string())
}

async fn read_positions_json(path: &Path) -> String {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Failed to read {}: {} (serving empty positions)",
                path.display(),
                e
            );
            r#"{"positions":{}}"#.to_string()
        }
    }
}

fn event_is_relevant(event: &notify::Event, watched: &Path) -> bool {
    if event.paths.is_empty() {
        return true;
    }

    let watched_canon = watched
        .canonicalize()
        .unwrap_or_else(|_| watched.to_path_buf());

    event.paths.iter().any(|p| {
        let p_canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        p_canon == watched_canon
            || (p_canon
                .file_name()
                .is_some_and(|n| watched.file_name().is_some_and(|wn| n == wn))
                && p_canon.parent() == watched_canon.parent())
    })
}

fn should_refresh(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Any
    )
}

async fn positions_stream(State(state): State<AppState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.positions_tx.subscribe();
    let stream = WatchStream::new(rx).map(|json| Ok(Event::default().event("positions").data(json)));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
}

async fn positions_snapshot(State(state): State<AppState>) -> impl IntoResponse {
    let json = state.positions_tx.borrow().clone();
    (
        [("content-type", "application/json; charset=utf-8")],
        json,
    )
}

async fn page_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn page_positions() -> Html<&'static str> {
    Html(POSITIONS_HTML)
}

#[derive(Debug, Deserialize)]
struct RefreshQuery {
    refresh: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct RefreshExternalQuery {
    refresh_external: Option<u8>,
}

async fn external_positions_debug(
    State(state): State<AppState>,
    Query(q): Query<RefreshQuery>,
) -> impl IntoResponse {
    let force = q.refresh == Some(1);
    let snapshot = get_external_snapshot(&state.external, force).await;
    Json(snapshot)
}

async fn positions_overview(
    State(state): State<AppState>,
    Query(q): Query<RefreshExternalQuery>,
) -> impl IntoResponse {
    let force = q.refresh_external == Some(1);

    // Internal: parse the current watched JSON.
    let internal_raw = state.positions_tx.borrow().clone();
    let internal: serde_json::Value = serde_json::from_str(&internal_raw).unwrap_or_else(|_| {
        serde_json::json!({ "positions": {} })
    });

    // External: cached fetch.
    let external = get_external_snapshot(&state.external, force).await;

    // Reconcile: best-effort (Kalshi only in v1).
    let reconciliation = reconcile_positions(&internal, &external);

    Json(OverviewResponse {
        snapshot_time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        internal,
        external,
        reconciliation,
    })
}

async fn markets_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_markets_browser_socket(socket, state))
}

async fn handle_markets_browser_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.markets_tx.subscribe();

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Ok(text) = msg else { break; };
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load local .env if present (server-side only).
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let port = parse_u16_env("MONITOR_PORT", 8080);
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();

    // Resolve static dir and positions.json path.
    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let positions_path = positions_path_from_env();
    let controller_ws = controller_debug_ws_from_env();

    let ttl_secs: u64 = std::env::var("EXTERNAL_POSITIONS_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15);

    let poly_user = std::env::var("POLY_POSITIONS_USER")
        .ok()
        .or_else(|| std::env::var("POLY_FUNDER").ok());

    // Initialize Kalshi client if credentials are present.
    let kalshi = match trading::kalshi::KalshiConfig::from_env() {
        Ok(cfg) => Some(trading::kalshi::KalshiApiClient::new(cfg)),
        Err(e) => {
            warn!("[external] Kalshi disabled (missing/invalid env): {}", e);
            None
        }
    };

    let external = Arc::new(ExternalState {
        ttl: Duration::from_secs(ttl_secs),
        cache: tokio::sync::RwLock::new(None),
        kalshi,
        http: reqwest::Client::new(),
        poly_user,
    });

    // Seed state with current file contents.
    let initial = read_positions_json(&positions_path).await;
    let (positions_tx, _positions_rx) = watch::channel::<String>(initial);
    let (markets_tx, _markets_rx) = broadcast::channel::<String>(1024);
    let state = AppState {
        positions_tx,
        markets_tx,
        external,
    };

    // Start file watcher that pushes updates into the watch channel.
    start_positions_watcher(positions_path, state.positions_tx.clone())
        .await
        .context("failed to start positions.json watcher")?;

    // Start controller debug ws connector to power Live Markets.
    tokio::spawn(connect_controller_loop(controller_ws, state.markets_tx.clone()));

    let app = Router::new()
        // Pages
        .route("/", get(page_index))
        .route("/positions", get(page_positions))
        // API
        .route("/api/positions", get(positions_snapshot))
        .route("/api/positions/stream", get(positions_stream))
        .route("/api/positions/external", get(external_positions_debug))
        .route("/api/positions/overview", get(positions_overview))
        .route("/api/markets/ws", get(markets_ws_handler))
        // Assets
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(state);

    info!("Monitor UI listening on http://{}/", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn get_external_snapshot(external: &ExternalState, force: bool) -> ExternalSnapshot {
    // Cache check.
    if !force {
        if let Some(hit) = external.cache.read().await.clone() {
            if hit.fetched_at.elapsed() < external.ttl {
                return hit.snapshot;
            }
        }
    }

    let mut errors = ExternalErrors::default();

    // Fetch in parallel (best-effort per provider).
    let kalshi_fut = async {
        let Some(client) = &external.kalshi else {
            return Err(anyhow::anyhow!("Kalshi client not configured (missing env)"));
        };
        let resp = client.get_positions().await?;
        Ok::<_, anyhow::Error>(normalize_kalshi_positions(resp))
    };

    let poly_fut = async {
        let Some(user) = &external.poly_user else {
            return Err(anyhow::anyhow!("POLY_FUNDER (or POLY_POSITIONS_USER) not set"));
        };
        let url = "https://data-api.polymarket.com/positions";
        let resp = external
            .http
            .get(url)
            .query(&[
                ("user", user.as_str()),
                ("sizeThreshold", "0"),
                ("limit", "500"),
                ("offset", "0"),
                ("sortBy", "TOKENS"),
                ("sortDirection", "DESC"),
            ])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Data API error {}: {}", status, body));
        }
        let positions: Vec<PolyDataPosition> = resp.json().await?;
        Ok::<_, anyhow::Error>(ExternalPolyBlock { positions })
    };

    let (kalshi_res, poly_res) = tokio::join!(kalshi_fut, poly_fut);

    let kalshi = match kalshi_res {
        Ok(k) => k,
        Err(e) => {
            errors.kalshi = Some(e.to_string());
            ExternalKalshiBlock { positions: vec![] }
        }
    };

    let polymarket = match poly_res {
        Ok(p) => p,
        Err(e) => {
            errors.polymarket = Some(e.to_string());
            ExternalPolyBlock { positions: vec![] }
        }
    };

    let snapshot = ExternalSnapshot {
        snapshot_time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        kalshi,
        polymarket,
        errors,
    };

    // Update cache.
    *external.cache.write().await = Some(CachedExternal {
        fetched_at: Instant::now(),
        snapshot: snapshot.clone(),
    });

    snapshot
}

fn normalize_kalshi_positions(resp: trading::kalshi::KalshiPositionsResponse) -> ExternalKalshiBlock {
    let mut out = Vec::with_capacity(resp.market_positions.len());
    for p in resp.market_positions {
        let net = p.position;
        let (yes, no) = if net >= 0 { (net, 0) } else { (0, -net) };
        out.push(ExternalKalshiPosition {
            ticker: p.ticker,
            net_position: net,
            yes_contracts: yes,
            no_contracts: no,
            total_traded: p.total_traded,
            market_exposure_cents: p.market_exposure,
            realized_pnl_cents: p.realized_pnl,
            fees_paid_cents: p.fees_paid,
        });
    }
    out.sort_by(|a, b| b.market_exposure_cents.cmp(&a.market_exposure_cents));
    ExternalKalshiBlock { positions: out }
}

fn reconcile_positions(internal: &serde_json::Value, external: &ExternalSnapshot) -> Reconciliation {
    let mut rec = Reconciliation {
        snapshot_time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ..Default::default()
    };

    // Build Kalshi lookup by ticker.
    let mut kalshi_by_ticker: std::collections::HashMap<&str, &ExternalKalshiPosition> =
        std::collections::HashMap::new();
    for p in &external.kalshi.positions {
        kalshi_by_ticker.insert(p.ticker.as_str(), p);
    }

    let positions_obj = internal.get("positions").and_then(|v| v.as_object());
    let Some(positions_obj) = positions_obj else {
        return rec;
    };

    let kalshi_api_down = external.errors.kalshi.is_some();

    for (market_id, p) in positions_obj {
        let internal_ky = json_f64(p, &["kalshi_yes", "contracts"]).unwrap_or(0.0);
        let internal_kn = json_f64(p, &["kalshi_no", "contracts"]).unwrap_or(0.0);

        let kalshi_ticker = extract_kalshi_ticker(market_id).or_else(|| {
            p.get("description")
                .and_then(|d| d.as_str())
                .and_then(extract_kalshi_ticker)
        });

        let mut status = "unknown_mapping".to_string();
        let mut reason: Option<String> = None;

        let mut external_yes: Option<i64> = None;
        let mut external_no: Option<i64> = None;
        let mut diff_yes: Option<f64> = None;
        let mut diff_no: Option<f64> = None;

        if kalshi_api_down {
            status = "api_error".to_string();
            reason = Some("kalshi_external_fetch_failed".to_string());
            rec.summary.api_error_count += 1;
        } else if let Some(ticker) = &kalshi_ticker {
            let ext = kalshi_by_ticker.get(ticker.as_str());
            let (ey, en) = match ext {
                Some(v) => (v.yes_contracts, v.no_contracts),
                None => (0, 0),
            };
            external_yes = Some(ey);
            external_no = Some(en);

            diff_yes = Some(internal_ky - ey as f64);
            diff_no = Some(internal_kn - en as f64);

            if (internal_ky - ey as f64).abs() < 0.0001 && (internal_kn - en as f64).abs() < 0.0001
            {
                status = "match".to_string();
                rec.summary.match_count += 1;
            } else {
                status = "mismatch".to_string();
                reason = Some("kalshi_contracts_mismatch".to_string());
                rec.summary.mismatch_count += 1;
            }
        } else {
            // No Kalshi ticker mapping available yet.
            rec.summary.unknown_mapping_count += 1;
        }

        rec.by_market_id.insert(
            market_id.clone(),
            ReconcileEntry {
                market_id: market_id.clone(),
                kalshi_ticker,
                internal_kalshi_yes: internal_ky,
                internal_kalshi_no: internal_kn,
                external_kalshi_yes: external_yes,
                external_kalshi_no: external_no,
                kalshi_diff_yes: diff_yes,
                kalshi_diff_no: diff_no,
                status,
                reason,
            },
        );
    }

    rec
}

fn extract_kalshi_ticker(s: &str) -> Option<String> {
    let idx = s.find("KX")?;
    let t = s[idx..].trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn json_f64(v: &serde_json::Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    cur.as_f64()
        .or_else(|| cur.as_i64().map(|x| x as f64))
        .or_else(|| cur.as_u64().map(|x| x as f64))
}

async fn connect_controller_loop(controller_ws: String, tx: broadcast::Sender<String>) {
    loop {
        match tokio_tungstenite::connect_async(&controller_ws).await {
            Ok((ws, _resp)) => {
                info!("[markets] connected to controller debug ws {}", controller_ws);
                let (mut write, mut read) = ws.split();

                let mut ping = tokio::time::interval(Duration::from_secs(20));
                loop {
                    tokio::select! {
                        _ = ping.tick() => {
                            let _ = write.send(tokio_tungstenite::tungstenite::Message::Ping(vec![])).await;
                        }
                        msg = read.next() => {
                            match msg {
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                    let _ = tx.send(text);
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bin))) => {
                                    if let Ok(text) = String::from_utf8(bin) {
                                        let _ = tx.send(text);
                                    }
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                                    warn!("[markets] controller debug ws closed; reconnecting...");
                                    break;
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => {
                                    warn!("[markets] controller debug ws error: {}; reconnecting...", e);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("[markets] failed to connect to controller debug ws: {}; retrying...", e);
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn start_positions_watcher(path: PathBuf, positions_tx: watch::Sender<String>) -> anyhow::Result<()> {
    // Watchers can fail if the file doesn't exist yet. In that case, watch the parent directory.
    let watch_target = if path.exists() {
        path.clone()
    } else {
        path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();

    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .context("failed to create file watcher")?;

    watcher
        .watch(&watch_target, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", watch_target.display()))?;

    let path_for_task = path.clone();
    tokio::spawn(async move {
        let mut pending_refresh: Option<tokio::task::JoinHandle<()>> = None;

        while let Some(res) = rx.recv().await {
            match res {
                Ok(event) => {
                    if !should_refresh(&event.kind) {
                        continue;
                    }
                    if !event_is_relevant(&event, &path_for_task) {
                        continue;
                    }

                    // Debounce bursts of file notifications.
                    if let Some(h) = pending_refresh.take() {
                        h.abort();
                    }

                    let path2 = path_for_task.clone();
                    let tx2 = positions_tx.clone();
                    pending_refresh = Some(tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let updated = read_positions_json(&path2).await;
                        if tx2.send(updated).is_err() {
                            // No receivers; fine.
                        }
                    }));
                }
                Err(e) => {
                    error!("watch error: {}", e);
                }
            }
        }
    });

    info!(
        "Watching {} for live updates (watch target: {})",
        path.display(),
        watch_target.display()
    );

    // Keep watcher alive by leaking into background task lifetime.
    // (The spawned task holds the mpsc receiver; we need the watcher to live too.)
    std::mem::forget(watcher);
    Ok(())
}

