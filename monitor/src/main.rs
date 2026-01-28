use std::{
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::{
        Html,
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::wrappers::WatchStream;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

#[derive(Clone)]
struct AppState {
    positions_tx: watch::Sender<String>,
    markets_tx: broadcast::Sender<String>,
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const POSITIONS_HTML: &str = include_str!("../static/positions.html");

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

    // Seed state with current file contents.
    let initial = read_positions_json(&positions_path).await;
    let (positions_tx, _positions_rx) = watch::channel::<String>(initial);
    let (markets_tx, _markets_rx) = broadcast::channel::<String>(1024);
    let state = AppState {
        positions_tx,
        markets_tx,
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
        .route("/api/markets/ws", get(markets_ws_handler))
        // Assets
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(state);

    info!("Monitor UI listening on http://{}/", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
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

