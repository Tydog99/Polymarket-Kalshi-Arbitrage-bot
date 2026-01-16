//! WebSocket client connection handler

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use url::Url;

use crate::protocol::{IncomingMessage, OutgoingMessage};

/// WebSocket connection manager
pub struct WebSocketClient {
    url: String,
    tx: Option<mpsc::UnboundedSender<OutgoingMessage>>,
}

impl WebSocketClient {
    /// Create a new WebSocket client
    pub fn new(url: String) -> Self {
        Self {
            url,
            tx: None,
        }
    }

    /// Connect to the WebSocket server and return channels for communication
    pub async fn connect(&mut self) -> Result<(
        mpsc::UnboundedSender<OutgoingMessage>,
        mpsc::UnboundedReceiver<IncomingMessage>,
    )> {
        let url = Url::parse(&self.url)
            .with_context(|| format!("Invalid WebSocket URL: {}", self.url))?;

        info!("[WS] Connecting to {}", url);

        let (ws_stream, _) = connect_async(&url)
            .await
            .with_context(|| format!("Failed to connect to {}", url))?;

        info!("[WS] Connected successfully");

        let (write, read) = ws_stream.split();

        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<IncomingMessage>();

        // Spawn task to handle outgoing messages
        let mut write = write;
        tokio::spawn(async move {
            while let Some(msg) = outgoing_rx.recv().await {
                match serde_json::to_string(&msg) {
                    Ok(json) => {
                        if let Err(e) = write.send(Message::Text(json)).await {
                            error!("[WS] Failed to send message: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("[WS] Failed to serialize message: {}", e);
                    }
                }
            }
        });

        // Spawn task to handle incoming messages
        let mut read = read;
        tokio::spawn(async move {
            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<IncomingMessage>(&text) {
                            Ok(msg) => {
                                if incoming_tx.send(msg).is_err() {
                                    warn!("[WS] Incoming channel closed");
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("[WS] Failed to parse message: {} - {}", e, text);
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("[WS] Connection closed by server");
                        break;
                    }
                    Ok(Message::Ping(_)) => {
                        // Handle ping (tungstenite handles pong automatically)
                        info!("[WS] Received ping");
                    }
                    Ok(Message::Pong(_)) => {
                        // Handle pong
                        info!("[WS] Received pong");
                    }
                    Err(e) => {
                        error!("[WS] WebSocket error: {}", e);
                        break;
                    }
                    _ => {
                        warn!("[WS] Unhandled message type");
                    }
                }
            }
            info!("[WS] Incoming message handler stopped");
        });

        self.tx = Some(outgoing_tx.clone());
        Ok((outgoing_tx, incoming_rx))
    }

    /// Reconnect to the WebSocket server
    #[allow(dead_code)] // Convenience API; current main loop reconnects externally
    pub async fn reconnect(&mut self) -> Result<()> {
        warn!("[WS] Attempting to reconnect...");
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        let (tx, _rx) = self.connect().await?;
        self.tx = Some(tx);
        Ok(())
    }
}
