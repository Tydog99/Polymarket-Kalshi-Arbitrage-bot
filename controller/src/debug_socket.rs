use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
pub struct DebugBroadcaster {
    tx: broadcast::Sender<String>,
}

impl DebugBroadcaster {
    pub fn send_json(&self, json: String) {
        // It's normal to have 0 receivers.
        let _ = self.tx.send(json);
    }
}

pub async fn spawn_debug_ws_server(addr: SocketAddr) -> Result<DebugBroadcaster> {
    let listener = TcpListener::bind(addr).await?;
    let (tx, _rx) = broadcast::channel::<String>(1024);
    let tx_for_task = tx.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => continue,
            };

            let mut rx = tx_for_task.subscribe();
            tokio::spawn(async move {
                let ws = match tokio_tungstenite::accept_async(stream).await {
                    Ok(ws) => ws,
                    Err(_) => return,
                };

                let (mut write, mut read) = ws.split();

                // Drain inbound messages (we don't currently accept commands, but
                // reading prevents some clients from stalling/closing weirdly).
                tokio::spawn(async move {
                    while let Some(Ok(_msg)) = read.next().await {}
                });

                while let Ok(msg) = rx.recv().await {
                    if write.send(Message::Text(msg)).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    Ok(DebugBroadcaster { tx })
}

