//! WebSocket server hosted by the controller.
//!
//! The remote trader connects as a *client* (so this works across NAT / "controller is reachable"),
//! then the controller sends `init` and `execute` messages.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{info, warn};

use crate::remote_protocol::{IncomingMessage, OutgoingMessage, Platform};

#[derive(Clone)]
pub struct RemoteTraderRouter {
    outgoing: Arc<RwLock<std::collections::HashMap<Platform, TraderSlot>>>,
}

#[derive(Clone)]
struct TraderSlot {
    conn_id: u64,
    tx: mpsc::UnboundedSender<IncomingMessage>,
}

impl RemoteTraderRouter {
    pub async fn is_connected(&self, platform: Platform) -> bool {
        self.outgoing.read().await.contains_key(&platform)
    }

    /// Send a message to the currently connected trader (if any).
    pub async fn try_send(&self, platform: Platform, msg: IncomingMessage) -> bool {
        self.outgoing
            .read()
            .await
            .get(&platform)
            .map(|slot| slot.tx.send(msg).is_ok())
            .unwrap_or(false)
    }

    pub async fn send(&self, platform: Platform, msg: IncomingMessage) -> Result<()> {
        let tx = self
            .outgoing
            .read()
            .await
            .get(&platform)
            .map(|slot| slot.tx.clone())
            .context("no trader connected for platform")?;
        tx.send(msg)
            .map_err(|_| anyhow::anyhow!("trader channel closed"))?;
        Ok(())
    }
}

impl RemoteTraderRouter {
    /// Test/debug helper: register a fake connected trader for a platform and get a receiver
    /// for messages sent by the controller. Useful for testing and debugging.
    pub async fn test_register(&self, platform: Platform) -> mpsc::UnboundedReceiver<IncomingMessage> {
        let (tx, rx) = mpsc::unbounded_channel::<IncomingMessage>();
        let mut map = self.outgoing.write().await;
        map.insert(
            platform,
            TraderSlot {
                conn_id: 0,
                tx,
            },
        );
        rx
    }
}

pub struct RemoteTraderServer {
    bind: SocketAddr,
    platforms: Vec<Platform>,
    dry_run: bool,
    router: RemoteTraderRouter,
}

impl RemoteTraderServer {
    pub fn new(bind: SocketAddr, platforms: Vec<Platform>, dry_run: bool) -> Self {
        Self {
            bind,
            platforms,
            dry_run,
            router: RemoteTraderRouter {
                outgoing: Arc::new(RwLock::new(std::collections::HashMap::new())),
            },
        }
    }

    pub fn router(&self) -> RemoteTraderRouter {
        self.router.clone()
    }

    pub async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(self.bind)
            .await
            .with_context(|| format!("bind remote trader ws server on {}", self.bind))?;
        info!(
            "[REMOTE] WebSocket server listening on {} (dry_run={})",
            self.bind, self.dry_run
        );

        let conn_counter = Arc::new(AtomicU64::new(1));

        loop {
            let (stream, peer) = listener.accept().await.context("accept trader tcp")?;
            let allowed_platforms = self.platforms.clone();
            let dry_run = self.dry_run;
            let router = self.router.clone();
            let conn_id = conn_counter.fetch_add(1, Ordering::Relaxed);

            tokio::spawn(async move {
                info!("[REMOTE] Trader connected from {} (conn_id={})", peer, conn_id);

                let ws = match accept_async(stream).await {
                    Ok(ws) => ws,
                    Err(e) => {
                        warn!("[REMOTE] accept trader websocket failed: {}", e);
                        return;
                    }
                };
                let (mut write, mut read) = ws.split();

                let (out_tx, mut out_rx) = mpsc::unbounded_channel::<IncomingMessage>();

                // Immediately init the trader (it should respond with init_ack including its platform).
                let _ = out_tx.send(IncomingMessage::Init {
                    platforms: allowed_platforms.clone(),
                    dry_run,
                });

                // Writer task
                let writer = tokio::spawn(async move {
                    while let Some(msg) = out_rx.recv().await {
                        match serde_json::to_string(&msg) {
                            Ok(json) => {
                                if let Err(e) = write.send(Message::Text(json)).await {
                                    return Err(anyhow::anyhow!("ws send failed: {}", e));
                                }
                            }
                            Err(e) => warn!("[REMOTE] serialize error: {}", e),
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                });

                // Reader task: registers this connection to a platform after init_ack.
                let out_tx_for_pong = out_tx.clone();
                let router_for_register = router.clone();
                let mut registered_platform: Option<Platform> = None;

                let reader = tokio::spawn(async move {
                    while let Some(frame) = read.next().await {
                        match frame {
                            Ok(Message::Text(text)) => match serde_json::from_str::<OutgoingMessage>(&text) {
                                Ok(OutgoingMessage::Ping { timestamp }) => {
                                    // Trader heartbeat ping → respond with pong so trader stays healthy.
                                    let _ = out_tx_for_pong.send(IncomingMessage::Pong { timestamp });
                                }
                                Ok(OutgoingMessage::InitAck { success, platforms: reported_platforms, error }) => {
                                    if !success {
                                        warn!(
                                            "[REMOTE] Trader init_ack failure (conn_id={}): {}",
                                            conn_id,
                                            error.unwrap_or_else(|| "unknown".to_string())
                                        );
                                        continue;
                                    }

                                    let Some(platform) = reported_platforms.first().copied() else {
                                        warn!("[REMOTE] Trader init_ack missing platform (conn_id={})", conn_id);
                                        continue;
                                    };

                                    // Validate platform is allowed by controller config.
                                    if !platforms_allowed(&allowed_platforms, platform) {
                                        warn!(
                                            "[REMOTE] Trader reported unsupported platform {:?} (conn_id={})",
                                            platform, conn_id
                                        );
                                        continue;
                                    }

                                    {
                                        let mut map = router_for_register.outgoing.write().await;
                                        if let Some(prev) = map.insert(
                                            platform,
                                            TraderSlot {
                                                conn_id,
                                                tx: out_tx_for_pong.clone(),
                                            },
                                        ) {
                                            warn!(
                                                "[REMOTE] Replacing trader for {:?}: prev_conn_id={} new_conn_id={}",
                                                platform, prev.conn_id, conn_id
                                            );
                                        } else {
                                            info!("[REMOTE] Registered trader for {:?} (conn_id={})", platform, conn_id);
                                        }
                                    }
                                    registered_platform = Some(platform);
                                }
                                Ok(OutgoingMessage::LegResult { market_id, leg_id, platform, success, latency_ns, error }) => {
                                    if success {
                                        info!(
                                            "[REMOTE] ✅ trader leg_result platform={:?} market_id={} leg_id={} latency={}µs",
                                            platform,
                                            market_id,
                                            leg_id,
                                            latency_ns / 1000
                                        );
                                    } else {
                                        warn!(
                                            "[REMOTE] ❌ trader leg_result platform={:?} market_id={} leg_id={} err={}",
                                            platform,
                                            market_id,
                                            leg_id,
                                            error.unwrap_or_else(|| "unknown".to_string())
                                        );
                                    }
                                }
                                Ok(OutgoingMessage::ExecutionResult { market_id, success, profit_cents, latency_ns, error }) => {
                                    // Legacy support: keep logging old execute results.
                                    if success {
                                        info!(
                                            "[REMOTE] ✅ trader execution_result market_id={} profit={}¢ latency={}µs",
                                            market_id,
                                            profit_cents,
                                            latency_ns / 1000
                                        );
                                    } else {
                                        warn!(
                                            "[REMOTE] ❌ trader execution_result market_id={} err={}",
                                            market_id,
                                            error.unwrap_or_else(|| "unknown".to_string())
                                        );
                                    }
                                }
                                Ok(other) => {
                                    info!("[REMOTE] trader msg: {:?}", other);
                                }
                                Err(e) => warn!("[REMOTE] parse error: {} (text={})", e, text),
                            },
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(e) => {
                                return Err(anyhow::anyhow!("ws read failed: {}", e));
                            }
                        }
                    }

                    // Unregister on disconnect (if we were registered)
                    if let Some(p) = registered_platform {
                        let mut map = router_for_register.outgoing.write().await;
                        if let Some(slot) = map.get(&p) {
                            if slot.conn_id == conn_id {
                                map.remove(&p);
                                warn!("[REMOTE] Trader disconnected: {:?} (conn_id={})", p, conn_id);
                            }
                        }
                    } else {
                        warn!("[REMOTE] Trader disconnected before init_ack (conn_id={})", conn_id);
                    }

                    Ok::<(), anyhow::Error>(())
                });

                let _ = tokio::select! {
                    r = reader => r,
                    w = writer => w,
                };
            });
        }
    }
}

fn platforms_allowed(allowed: &[Platform], platform: Platform) -> bool {
    allowed.iter().any(|p| *p == platform)
}

