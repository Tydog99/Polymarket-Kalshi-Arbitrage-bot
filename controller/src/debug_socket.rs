use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

use crate::arb::kalshi_fee;
use crate::types::{GlobalState, MarketType, PriceCents};

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

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(serde::Serialize)]
struct MarketDetailWs {
    market_id: u16,
    description: String,
    league: String,
    market_type: String,
    k_yes: u16,
    k_no: u16,
    p_yes: u16,
    p_no: u16,
    gap_cents: Option<i16>,
    yes_size: u16,
    no_size: u16,
    k_updates: u32,
    p_updates: u32,
    k_last_ms: u64,
    p_last_ms: u64,
}

fn market_type_str(mt: MarketType) -> &'static str {
    match mt {
        MarketType::Moneyline => "moneyline",
        MarketType::Spread => "spread",
        MarketType::Total => "total",
        MarketType::Btts => "btts",
    }
}

fn compute_gap_and_sizes(
    threshold_cents: PriceCents,
    k_yes: u16,
    k_no: u16,
    k_yes_size: u16,
    k_no_size: u16,
    p_yes: u16,
    p_no: u16,
    p_yes_size: u16,
    p_no_size: u16,
) -> (Option<i16>, u16, u16) {
    let has_k = k_yes > 0 && k_no > 0;
    let has_p = p_yes > 0 && p_no > 0;
    if !(has_k && has_p) {
        return (None, 0, 0);
    }

    let fee1 = kalshi_fee(k_no);
    let cost1 = p_yes + k_no + fee1;
    let fee2 = kalshi_fee(k_yes);
    let cost2 = k_yes + fee2 + p_no;

    if cost1 <= cost2 {
        (
            Some(cost1 as i16 - threshold_cents as i16),
            p_yes_size,
            k_no_size,
        )
    } else {
        (
            Some(cost2 as i16 - threshold_cents as i16),
            k_yes_size,
            p_no_size,
        )
    }
}

fn build_market_detail(state: &GlobalState, market_id: u16) -> Option<MarketDetailWs> {
    let market = state.get_by_id(market_id)?;
    let pair = market.pair()?;

    let (k_yes, k_no, k_yes_size, k_no_size) = market.kalshi.load();
    let (p_yes, p_no, p_yes_size, p_no_size) = market.poly.load();
    let (k_updates, p_updates) = market.load_update_counts();
    let (k_last_ms, p_last_ms) = market.last_updates_unix_ms();

    let threshold = state.arb_config().threshold_cents();
    let (gap_cents, yes_size, no_size) = compute_gap_and_sizes(
        threshold,
        k_yes,
        k_no,
        k_yes_size,
        k_no_size,
        p_yes,
        p_no,
        p_yes_size,
        p_no_size,
    );

    Some(MarketDetailWs {
        market_id,
        description: pair.description.to_string(),
        league: pair.league.to_string(),
        market_type: market_type_str(pair.market_type).to_string(),
        k_yes,
        k_no,
        p_yes,
        p_no,
        gap_cents,
        yes_size,
        no_size,
        k_updates,
        p_updates,
        k_last_ms,
        p_last_ms,
    })
}

/// Build a JSON snapshot for the web UI. Includes totals and deltas since the last call.
pub fn build_snapshot_json(state: &GlobalState, prev_k: &mut u64, prev_p: &mut u64) -> String {
    #[derive(serde::Serialize)]
    struct Snapshot {
        r#type: &'static str,
        now_ms: u64,
        market_count: usize,
        threshold_cents: u16,
        with_both_prices: usize,
        kalshi_total_updates: u64,
        polymarket_total_updates: u64,
        kalshi_delta: u64,
        polymarket_delta: u64,
        markets: Vec<MarketDetailWs>,
    }

    let now_ms = now_unix_ms();
    let market_count = state.market_count();
    let threshold_cents = state.arb_config().threshold_cents();

    let mut total_k: u64 = 0;
    let mut total_p: u64 = 0;
    let mut with_both: usize = 0;
    let mut markets: Vec<MarketDetailWs> = Vec::with_capacity(market_count);

    for m in state.markets.iter().take(market_count) {
        let (k_upd, p_upd) = m.load_update_counts();
        total_k += k_upd as u64;
        total_p += p_upd as u64;

        let (k_yes, k_no, _, _) = m.kalshi.load();
        let (p_yes, p_no, _, _) = m.poly.load();
        if k_yes > 0 && k_no > 0 && p_yes > 0 && p_no > 0 {
            with_both += 1;
        }

        if let Some(pair) = m.pair() {
            markets.push(
                build_market_detail(state, m.market_id)
                    // pair exists, so build_market_detail should succeed; fallback if it doesn't.
                    .unwrap_or(MarketDetailWs {
                        market_id: m.market_id,
                        description: pair.description.to_string(),
                        league: pair.league.to_string(),
                        market_type: market_type_str(pair.market_type).to_string(),
                        k_yes,
                        k_no,
                        p_yes,
                        p_no,
                        gap_cents: None,
                        yes_size: 0,
                        no_size: 0,
                        k_updates: k_upd,
                        p_updates: p_upd,
                        k_last_ms: m.last_updates_unix_ms().0,
                        p_last_ms: m.last_updates_unix_ms().1,
                    }),
            );
        }
    }

    let kalshi_delta = total_k.saturating_sub(*prev_k);
    let polymarket_delta = total_p.saturating_sub(*prev_p);
    *prev_k = total_k;
    *prev_p = total_p;

    let snapshot = Snapshot {
        r#type: "snapshot",
        now_ms,
        market_count,
        threshold_cents,
        with_both_prices: with_both,
        kalshi_total_updates: total_k,
        polymarket_total_updates: total_p,
        kalshi_delta,
        polymarket_delta,
        markets,
    };

    serde_json::to_string(&snapshot).unwrap_or_else(|_| "{\"type\":\"snapshot\"}".to_string())
}

/// Build a JSON "market_update" message for a single market.
pub fn build_market_update_json(state: &GlobalState, market_id: u16) -> Option<String> {
    #[derive(serde::Serialize)]
    struct MarketUpdate {
        r#type: &'static str,
        now_ms: u64,
        threshold_cents: u16,
        market: MarketDetailWs,
    }

    let market = build_market_detail(state, market_id)?;
    let msg = MarketUpdate {
        r#type: "market_update",
        now_ms: now_unix_ms(),
        threshold_cents: state.arb_config().threshold_cents(),
        market,
    };

    serde_json::to_string(&msg).ok()
}


