use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,debug_web=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let bind: SocketAddr = std::env::var("DEBUG_WEB_BIND")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_string())
        .parse()?;

    let controller_ws = std::env::var("CONTROLLER_DEBUG_WS")
        .unwrap_or_else(|_| "ws://127.0.0.1:9105".to_string());

    let (tx, _rx) = broadcast::channel::<String>(1024);
    let state = Arc::new(AppState { tx });

    tokio::spawn(connect_controller_loop(
        controller_ws.clone(),
        state.clone(),
    ));

    let static_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let index_file = static_dir.join("index.html");

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state)
        .nest_service("/", ServeDir::new(static_dir))
        .fallback_service(ServeFile::new(index_file));

    info!("debug_web listening on http://{}", bind);
    info!("proxying controller stream from {}", controller_ws);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_browser_socket(socket, state))
}

async fn handle_browser_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.tx.subscribe();

    loop {
        tokio::select! {
            // Forward controller snapshots -> browser
            msg = rx.recv() => {
                let Ok(text) = msg else { break; };
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            // Drain inbound browser messages (ignore; allows clean close)
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

async fn connect_controller_loop(controller_ws: String, state: Arc<AppState>) {
    loop {
        match tokio_tungstenite::connect_async(&controller_ws).await {
            Ok((ws, _resp)) => {
                info!("connected to controller debug ws");
                let (mut write, mut read) = ws.split();

                // Keep the connection alive (some proxies close idle ws).
                let mut ping = tokio::time::interval(Duration::from_secs(20));

                loop {
                    tokio::select! {
                        _ = ping.tick() => {
                            let _ = write.send(tokio_tungstenite::tungstenite::Message::Ping(vec![])).await;
                        }
                        msg = read.next() => {
                            match msg {
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                    let _ = state.tx.send(text);
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bin))) => {
                                    if let Ok(text) = String::from_utf8(bin) {
                                        let _ = state.tx.send(text);
                                    }
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                                    warn!("controller debug ws closed; reconnecting...");
                                    break;
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => {
                                    warn!("controller debug ws error: {}; reconnecting...", e);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("failed to connect to controller debug ws: {}; retrying...", e);
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

