//! Remote trading bot - receives trading instructions via WebSocket

mod config;
mod heartbeat;
mod paths;
mod protocol;
mod trader;
mod websocket;

use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{error, info, warn};

use config::Config;
use protocol::IncomingMessage;
use trader::Trader;
use websocket::WebSocketClient;

/// Discover controller via Tailscale beacon or use configured URL
async fn resolve_websocket_url(config: &Config) -> Result<String> {
    // If WEBSOCKET_URL is set, use it directly
    if let Some(url) = &config.websocket_url {
        info!("[MAIN] Using configured WEBSOCKET_URL: {}", url);
        return Ok(url.clone());
    }

    // Otherwise, use Tailscale beacon discovery
    info!("[MAIN] No WEBSOCKET_URL set, using Tailscale beacon discovery...");

    // Verify Tailscale is connected
    let status = match tailscale::verify::verify() {
        Ok(s) => s,
        Err(e) => {
            error!("[MAIN] Tailscale not available: {}", e);
            error!("[MAIN] To use auto-discovery, set up Tailscale:");
            error!("[MAIN]   1. Install: brew install tailscale");
            error!("[MAIN]   2. Start:   sudo tailscaled");
            error!("[MAIN]   3. Login:   tailscale up");
            error!("[MAIN]   4. Verify:  tailscale status");
            error!("[MAIN] Or set WEBSOCKET_URL to connect directly:");
            error!("[MAIN]   WEBSOCKET_URL=ws://<controller-ip>:9001");
            anyhow::bail!("Tailscale verification failed");
        }
    };
    info!("[MAIN] Tailscale connected as {}", status.self_ip);

    // Load beacon config
    let ts_config = match tailscale::Config::load() {
        Ok(c) => c,
        Err(_) => {
            error!("[MAIN] Config file ~/.arb/config.toml not found");
            error!("[MAIN] Run the bootstrap tool to create it:");
            error!("[MAIN]   cargo run -p bootstrap");
            error!("[MAIN] Or set WEBSOCKET_URL to skip auto-discovery:");
            error!("[MAIN]   WEBSOCKET_URL=ws://<controller-ip>:9001");
            anyhow::bail!("Config file not found - run bootstrap first");
        }
    };

    // Listen for controller beacon
    info!("[MAIN] Waiting for controller beacon on port {}...", ts_config.beacon_port);
    let listener = tailscale::BeaconListener::new(ts_config.beacon_port).await?;

    let controller = match listener
        .wait_for_controller_timeout(std::time::Duration::from_secs(300))
        .await
    {
        Ok(c) => c,
        Err(_) => {
            error!("[MAIN] Timeout waiting for controller beacon");
            error!("[MAIN] Ensure the controller is running with REMOTE_TRADER=1");
            error!("[MAIN] Or set WEBSOCKET_URL to connect directly:");
            error!("[MAIN]   WEBSOCKET_URL=ws://<controller-ip>:9001");
            anyhow::bail!("Timeout waiting for controller");
        }
    };

    let url = format!("ws://{}:{}", controller.ip, controller.ws_port);
    info!("[MAIN] Discovered controller at {}", url);

    Ok(url)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load environment variables from `.env` (supports workspace-root `.env`)
    paths::load_dotenv();

    info!("[MAIN] Starting remote trader...");
    let one_shot = std::env::var("ONE_SHOT")
        .map(|v| v == "1" || v == "true" || v == "yes")
        .unwrap_or(false);

    // Load configuration
    let config = Arc::new(Config::from_env().context("Failed to load configuration")?);
    info!("[MAIN] Configuration loaded (dry_run={})", config.dry_run);

    // Create trader
    let mut trader = Trader::new(config.clone());

    // Main event loop with reconnection logic
    loop {
        // Resolve WebSocket URL at the start of each connection attempt
        let ws_url = match resolve_websocket_url(&config).await {
            Ok(url) => url,
            Err(e) => {
                error!("[MAIN] Failed to resolve WebSocket URL: {}", e);
                warn!("[MAIN] Retrying in 5 seconds...");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        // Create WebSocket client with resolved URL
        let mut ws_client = WebSocketClient::new(ws_url);

        match ws_client.connect().await {
            Ok((outgoing_tx, mut incoming_rx)) => {
                info!("[MAIN] WebSocket connected, starting main loop");

                // Start heartbeat manager
                let heartbeat = Arc::new(heartbeat::HeartbeatManager::new(outgoing_tx.clone(), 5, 15));
                let heartbeat_handle = heartbeat.start();

                // Main message processing loop
                let mut connection_healthy = true;
                while connection_healthy {
                    tokio::select! {
                        // Handle incoming messages
                        msg = incoming_rx.recv() => {
                            match msg {
                                Some(IncomingMessage::Ping { timestamp }) => {
                                    // Host sent ping, respond with pong and update heartbeat
                                    heartbeat.record_pong();
                                    let response = trader.handle_message(IncomingMessage::Ping { timestamp }).await;
                                    if outgoing_tx.send(response).is_err() {
                                        warn!("[MAIN] Failed to send pong - connection may be closed");
                                        connection_healthy = false;
                                    }
                                }
                                Some(IncomingMessage::Pong { timestamp: _ }) => {
                                    // Host responded to our ping
                                    heartbeat.record_pong();
                                }
                                Some(msg) => {
                                    let is_execute = matches!(msg, IncomingMessage::Execute { .. });
                                    let response = trader.handle_message(msg).await;
                                    if outgoing_tx.send(response).is_err() {
                                        warn!("[MAIN] Failed to send response - connection may be closed");
                                        connection_healthy = false;
                                    }
                                    if one_shot && is_execute {
                                        info!("[MAIN] ONE_SHOT enabled; exiting after first execute");
                                        return Ok(());
                                    }
                                }
                                None => {
                                    warn!("[MAIN] Incoming channel closed");
                                    connection_healthy = false;
                                }
                            }
                        }
                        // Check for heartbeat timeout
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                            if heartbeat.is_timed_out() {
                                warn!("[MAIN] Heartbeat timeout detected");
                                connection_healthy = false;
                            }
                        }
                    }
                }

                // Stop heartbeat
                heartbeat_handle.abort();
                info!("[MAIN] Connection lost, will attempt to reconnect...");
            }
            Err(e) => {
                error!("[MAIN] Failed to connect: {}", e);
                warn!("[MAIN] Retrying in 5 seconds...");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}
