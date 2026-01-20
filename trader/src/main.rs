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

// For positions command
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::RsaPrivateKey;
use trading::kalshi::{KalshiApiClient, KalshiConfig};

fn parse_bool_env(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v == "true" || v == "yes")
        .unwrap_or(false)
}

/// Minimal CLI parser: expects `--key value` pairs.
fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn usage_manual_trade() -> String {
    [
        "Manual trade mode (simulates controller messages):",
        "",
        "Polymarket order:",
        "  TRADER_PLATFORM=polymarket cargo run -p remote-trader --release -- manual-trade \\",
        "    --poly-token <CLOB_TOKEN_ID> \\",
        "    --side <yes|no> \\",
        "    --limit-price-cents <0-100> \\",
        "    --contracts <INT>",
        "",
        "Kalshi order:",
        "  TRADER_PLATFORM=kalshi cargo run -p remote-trader --release -- manual-trade \\",
        "    --kalshi-ticker <TICKER> \\",
        "    --side <yes|no> \\",
        "    --limit-price-cents <0-100> \\",
        "    --contracts <INT>",
        "",
        "Sell order (requires existing position):",
        "  TRADER_PLATFORM=kalshi cargo run -p remote-trader --release -- manual-trade \\",
        "    --kalshi-ticker <TICKER> \\",
        "    --action sell \\",
        "    --side <yes|no> \\",
        "    --limit-price-cents <0-100> \\",
        "    --contracts <INT>",
        "",
        "Or specify a USD spend amount (requires limit price):",
        "",
        "  cargo run -p remote-trader --release -- manual-trade \\",
        "    --poly-token <CLOB_TOKEN_ID> \\",
        "    --side <yes|no> \\",
        "    --limit-price-cents <0-100> \\",
        "    --spend-usd <FLOAT>",
        "",
        "Notes:",
        "  - Live trading requires DRY_RUN=0 and platform creds in env.",
        "  - TRADER_PLATFORM must match the market identifier you provide:",
        "    - --poly-token requires TRADER_PLATFORM=polymarket",
        "    - --kalshi-ticker requires TRADER_PLATFORM=kalshi",
        "  - `--side` is for logging; the token/ticker determines the outcome you trade.",
        "  - `--action` defaults to 'buy' if not specified.",
    ]
    .join("\n")
}

async fn maybe_run_manual_trade(config: Arc<Config>) -> Result<Option<()>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Enable via either: `manual-trade` subcommand OR env var.
    let is_manual = args.first().is_some_and(|a| a == "manual-trade")
        || parse_bool_env("MANUAL_TRADE");
    if !is_manual {
        return Ok(None);
    }

    if has_flag(&args, "--help") || has_flag(&args, "-h") {
        println!("{}", usage_manual_trade());
        return Ok(Some(()));
    }

    // Parse market identifier (either --poly-token or --kalshi-ticker)
    let poly_token = arg_value(&args, "--poly-token");
    let kalshi_ticker = arg_value(&args, "--kalshi-ticker");

    // Determine platform based on which identifier was provided
    let (platform, market_identifier) = match (&poly_token, &kalshi_ticker) {
        (Some(token), None) => (crate::protocol::Platform::Polymarket, token.clone()),
        (None, Some(ticker)) => (crate::protocol::Platform::Kalshi, ticker.clone()),
        (Some(_), Some(_)) => {
            anyhow::bail!(
                "Cannot specify both --poly-token and --kalshi-ticker\n\n{}",
                usage_manual_trade()
            );
        }
        (None, None) => {
            anyhow::bail!(
                "Missing --poly-token or --kalshi-ticker\n\n{}",
                usage_manual_trade()
            );
        }
    };

    // Validate that TRADER_PLATFORM matches the provided market identifier
    if config.platform != platform {
        anyhow::bail!(
            "TRADER_PLATFORM={:?} does not match the provided market identifier.\n\
             Use --poly-token with TRADER_PLATFORM=polymarket, or\n\
             use --kalshi-ticker with TRADER_PLATFORM=kalshi",
            config.platform
        );
    }

    // Parse action (buy or sell), default to buy
    let action_str = arg_value(&args, "--action").unwrap_or_else(|| "buy".to_string());
    let action = match action_str.as_str() {
        "buy" | "BUY" | "Buy" => crate::protocol::OrderAction::Buy,
        "sell" | "SELL" | "Sell" => crate::protocol::OrderAction::Sell,
        other => anyhow::bail!("Invalid --action: {} (expected buy|sell)", other),
    };

    let side_str = arg_value(&args, "--side")
        .ok_or_else(|| anyhow::anyhow!("Missing --side\n\n{}", usage_manual_trade()))?;
    let side = match side_str.as_str() {
        "yes" | "YES" | "Yes" => crate::protocol::OutcomeSide::Yes,
        "no" | "NO" | "No" => crate::protocol::OutcomeSide::No,
        other => anyhow::bail!("Invalid --side: {} (expected yes|no)", other),
    };

    let limit_price_cents: u16 = arg_value(&args, "--limit-price-cents")
        .ok_or_else(|| anyhow::anyhow!("Missing --limit-price-cents\n\n{}", usage_manual_trade()))?
        .parse()
        .context("Failed to parse --limit-price-cents as integer")?;
    if limit_price_cents > 100 {
        anyhow::bail!("--limit-price-cents must be 0..=100");
    }

    let contracts: i64 = if let Some(c) = arg_value(&args, "--contracts") {
        c.parse().context("Failed to parse --contracts as integer")?
    } else if let Some(spend) = arg_value(&args, "--spend-usd") {
        let spend_usd: f64 = spend.parse().context("Failed to parse --spend-usd as float")?;
        let px = (limit_price_cents as f64) / 100.0;
        if px <= 0.0 {
            anyhow::bail!("--limit-price-cents must be > 0 when using --spend-usd");
        }
        let c = (spend_usd / px).floor() as i64;
        c
    } else {
        return Err(anyhow::anyhow!(
            "Missing --contracts or --spend-usd\n\n{}",
            usage_manual_trade()
        ));
    };

    if contracts < 1 {
        anyhow::bail!("Computed contracts < 1; increase --contracts or --spend-usd");
    }

    info!(
        "[MANUAL] Starting manual-trade: platform={:?} market={} action={:?} side={:?} limit={}c contracts={} dry_run={}",
        platform, market_identifier, action, side, limit_price_cents, contracts, config.dry_run
    );

    // Simulate controller Init + ExecuteLeg messages (same trader codepath).
    let mut trader = Trader::new(config.clone());

    let init_ack = trader
        .handle_message(IncomingMessage::Init {
            platforms: vec![platform],
            dry_run: config.dry_run,
        })
        .await;
    info!("[MANUAL] init_ack={:?}", init_ack);

    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let leg_id = format!("manual-{}", ms);
    let res = trader
        .handle_message(IncomingMessage::ExecuteLeg {
            market_id: 0,
            leg_id,
            platform,
            action,
            side,
            price: limit_price_cents,
            contracts,
            kalshi_market_ticker: kalshi_ticker,
            poly_token,
            pair_id: Some("MANUAL_TRADE".to_string()),
            description: Some("manual-trade".to_string()),
        })
        .await;

    info!("[MANUAL] execute_leg_result={:?}", res);
    Ok(Some(()))
}

/// Show Kalshi portfolio positions
async fn maybe_run_positions(config: &Config) -> Result<Option<()>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Check for positions subcommand
    if args.first().map(|a| a.as_str()) != Some("positions") {
        return Ok(None);
    }

    // Require Kalshi credentials
    let api_key = config
        .kalshi_api_key
        .clone()
        .ok_or_else(|| anyhow::anyhow!("KALSHI_API_KEY_ID required for positions command"))?;
    let private_key_pem = config
        .kalshi_private_key
        .clone()
        .ok_or_else(|| anyhow::anyhow!("KALSHI_PRIVATE_KEY_PATH required for positions command"))?;

    let private_key = RsaPrivateKey::from_pkcs1_pem(private_key_pem.trim())
        .map_err(|e| anyhow::anyhow!("Failed to parse Kalshi private key: {}", e))?;

    let kalshi_cfg = KalshiConfig {
        api_key_id: api_key,
        private_key,
    };
    let client = KalshiApiClient::new(kalshi_cfg);

    println!("Fetching Kalshi positions...\n");

    let positions = client.get_positions().await?;

    if positions.market_positions.is_empty() {
        println!("No open positions found.");
        return Ok(Some(()));
    }

    println!("{:<35} {:>10} {:>12} {:>12}", "TICKER", "POSITION", "EXPOSURE", "P&L");
    println!("{}", "-".repeat(75));

    for pos in &positions.market_positions {
        let side = if pos.position > 0 { "YES" } else { "NO" };
        let qty = pos.position.abs();
        println!(
            "{:<35} {:>6} {:>3} {:>10.2}¢ {:>10.2}¢",
            pos.ticker,
            qty,
            side,
            pos.market_exposure as f64 / 100.0,
            pos.realized_pnl as f64 / 100.0,
        );
    }

    println!("\n{} position(s) found.", positions.market_positions.len());
    println!("\nTo sell a position, use:");
    println!("  cargo run -p remote-trader --release -- manual-trade \\");
    println!("    --kalshi-ticker <TICKER> \\");
    println!("    --action sell \\");
    println!("    --side <yes|no> \\");
    println!("    --limit-price-cents <PRICE> \\");
    println!("    --contracts <COUNT>");

    Ok(Some(()))
}

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

    // Initialize HTTP capture session if CAPTURE_DIR is set
    // Must be done early, before any HTTP clients are created
    match trading::capture::init_capture_session() {
        Ok(Some(session)) => {
            info!(
                "[MAIN] Capture mode active: {} (filter: {})",
                session.session_dir.display(),
                session.filter
            );
        }
        Ok(None) => {
            // Capture disabled - nothing to log
        }
        Err(e) => {
            error!("[MAIN] Failed to initialize capture session: {}", e);
            error!("[MAIN] Check that CAPTURE_DIR points to a writable directory");
            return Err(e.into());
        }
    }

    info!("[MAIN] Starting remote trader...");
    let one_shot = std::env::var("ONE_SHOT")
        .map(|v| v == "1" || v == "true" || v == "yes")
        .unwrap_or(false);

    // Load configuration
    let config = Arc::new(Config::from_env().context("Failed to load configuration")?);
    info!("[MAIN] Configuration loaded (dry_run={})", config.dry_run);

    // Optional "positions" command to query Kalshi portfolio
    if let Some(()) = maybe_run_positions(&config).await? {
        return Ok(());
    }

    // Optional "manual trade" mode for validating live execution without a controller.
    if let Some(()) = maybe_run_manual_trade(config.clone()).await? {
        return Ok(());
    }

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
