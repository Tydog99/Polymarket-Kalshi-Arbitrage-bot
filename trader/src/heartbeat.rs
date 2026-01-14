//! Heartbeat management for connection health monitoring

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tracing::{error, info, warn};

use crate::protocol::OutgoingMessage;

/// Heartbeat manager that sends ping messages and monitors connection health
pub struct HeartbeatManager {
    ping_interval: Duration,
    timeout: Duration,
    last_pong: Arc<AtomicU64>,
    outgoing_tx: mpsc::UnboundedSender<OutgoingMessage>,
}

impl HeartbeatManager {
    /// Create a new heartbeat manager
    pub fn new(
        outgoing_tx: mpsc::UnboundedSender<OutgoingMessage>,
        ping_interval_secs: u64,
        timeout_secs: u64,
    ) -> Self {
        Self {
            ping_interval: Duration::from_secs(ping_interval_secs),
            timeout: Duration::from_secs(timeout_secs),
            last_pong: Arc::new(AtomicU64::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )),
            outgoing_tx,
        }
    }

    /// Update the last pong timestamp
    pub fn record_pong(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.last_pong.store(now, Ordering::Release);
    }

    /// Check if connection has timed out
    pub fn is_timed_out(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let last_pong = self.last_pong.load(Ordering::Acquire);
        let elapsed = now.saturating_sub(last_pong);
        elapsed > self.timeout.as_secs()
    }

    /// Start the heartbeat loop
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let last_pong = self.last_pong.clone();
        let outgoing_tx = self.outgoing_tx.clone();
        let ping_interval = self.ping_interval;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(ping_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                // Check for timeout
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let last_pong_ts = last_pong.load(Ordering::Acquire);
                let elapsed = now.saturating_sub(last_pong_ts);

                if elapsed > 15 {
                    warn!("[HEARTBEAT] Connection timeout detected ({}s since last pong)", elapsed);
                    // Connection is considered unhealthy, but we continue sending pings
                    // The main loop should handle reconnection
                }

                // Send ping
                let ping = OutgoingMessage::Ping {
                    timestamp: now,
                };
                if outgoing_tx.send(ping).is_err() {
                    error!("[HEARTBEAT] Failed to send ping - channel closed");
                    break;
                }
                info!("[HEARTBEAT] Sent ping (timestamp: {})", now);
            }
            info!("[HEARTBEAT] Heartbeat loop stopped");
        })
    }
}
