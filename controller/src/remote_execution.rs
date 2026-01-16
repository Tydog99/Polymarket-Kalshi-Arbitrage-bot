//! Execution path that sends trades to a remote trader over WebSocket.

use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::circuit_breaker::CircuitBreaker;
use crate::remote_protocol::{IncomingMessage, OrderAction, OutcomeSide, Platform as WsPlatform};
use crate::remote_trader::RemoteTraderRouter;
use crate::types::{ArbType, FastExecutionRequest, GlobalState, MarketPair};

pub struct RemoteExecutor {
    state: Arc<GlobalState>,
    circuit_breaker: Arc<CircuitBreaker>,
    trader: RemoteTraderRouter,
    in_flight: Arc<[AtomicU64; 8]>,
    leg_seq: AtomicU64,
    pub dry_run: bool,
}

impl RemoteExecutor {
    pub fn new(
        state: Arc<GlobalState>,
        circuit_breaker: Arc<CircuitBreaker>,
        trader: RemoteTraderRouter,
        dry_run: bool,
    ) -> Self {
        Self {
            state,
            circuit_breaker,
            trader,
            in_flight: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            leg_seq: AtomicU64::new(1),
            dry_run,
        }
    }

    fn release_in_flight_delayed(&self, market_id: u16) {
        if market_id < 512 {
            let in_flight = self.in_flight.clone();
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                let mask = !(1u64 << bit);
                in_flight[slot].fetch_and(mask, Ordering::Release);
            });
        }
    }

    pub async fn process(&self, req: FastExecutionRequest) -> Result<()> {
        let market_id = req.market_id;

        // Deduplication check (512 markets via 8x u64 bitmask)
        if market_id < 512 {
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            let mask = 1u64 << bit;
            let prev = self.in_flight[slot].fetch_or(mask, Ordering::AcqRel);
            if prev & mask != 0 {
                return Ok(());
            }
        }

        let market = self
            .state
            .get_by_id(market_id)
            .ok_or_else(|| anyhow!("Unknown market_id {}", market_id))?;
        let pair = market
            .pair()
            .ok_or_else(|| anyhow!("No pair for market_id {}", market_id))?;

        let profit_cents = req.profit_cents();
        if profit_cents < 1 {
            self.release_in_flight_delayed(market_id);
            return Ok(());
        }

        let max_contracts = (req.yes_size.min(req.no_size) / 100) as i64;
        if max_contracts < 1 {
            self.release_in_flight_delayed(market_id);
            return Ok(());
        }

        // Circuit breaker check
        if let Err(_reason) = self
            .circuit_breaker
            .can_execute(&pair.pair_id, max_contracts)
            .await
        {
            self.release_in_flight_delayed(market_id);
            return Ok(());
        }

        info!(
            "[REMOTE_EXEC] 🎯 {} | {:?} y={}¢ n={}¢ | profit={}¢ | {}x",
            pair.description, req.arb_type, req.yes_price, req.no_price, profit_cents, max_contracts
        );

        if self.dry_run {
            info!(
                "[REMOTE_EXEC] 🏃 DRY RUN - sending execute to trader (no real orders)"
            );
        }

        // Build legs for this arb, then only dispatch if required remote traders are available.
        let legs = build_legs(&req, &pair, max_contracts, &self.leg_seq);
        if legs.is_empty() {
            self.release_in_flight_delayed(market_id);
            return Ok(());
        }

        // Enforce availability: cross-platform arbs require both remote traders.
        if !can_dispatch(&self.trader, req.arb_type).await {
            warn!(
                "[REMOTE_EXEC] Missing remote trader(s) for {:?}; dropping arb market_id={}",
                req.arb_type, market_id
            );
            self.release_in_flight_delayed(market_id);
            return Ok(());
        }

        for (platform, msg) in legs {
            if !self.trader.try_send(platform, msg).await {
                warn!(
                    "[REMOTE_EXEC] Failed to send leg to {:?}; trader not connected? market_id={}",
                    platform, market_id
                );
            }
        }

        self.release_in_flight_delayed(market_id);
        Ok(())
    }
}

async fn can_dispatch(router: &RemoteTraderRouter, arb_type: ArbType) -> bool {
    match arb_type {
        ArbType::PolyYesKalshiNo | ArbType::KalshiYesPolyNo => {
            router.is_connected(WsPlatform::Kalshi).await && router.is_connected(WsPlatform::Polymarket).await
        }
        ArbType::PolyOnly => router.is_connected(WsPlatform::Polymarket).await,
        ArbType::KalshiOnly => router.is_connected(WsPlatform::Kalshi).await,
    }
}

fn build_legs(
    req: &FastExecutionRequest,
    pair: &MarketPair,
    contracts: i64,
    leg_seq: &AtomicU64,
) -> Vec<(WsPlatform, IncomingMessage)> {
    let mk_leg_id = |suffix: &str| -> String {
        let n = leg_seq.fetch_add(1, Ordering::Relaxed);
        format!("m{}-{}-{}", req.market_id, n, suffix)
    };

    let pair_id = Some(pair.pair_id.to_string());
    let description = Some(pair.description.to_string());

    match req.arb_type {
        // Cross-platform: Poly YES + Kalshi NO
        ArbType::PolyYesKalshiNo => vec![
            (
                WsPlatform::Polymarket,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("poly-yes"),
                    platform: WsPlatform::Polymarket,
                    action: OrderAction::Buy,
                    side: OutcomeSide::Yes,
                    price: req.yes_price,
                    contracts,
                    kalshi_market_ticker: None,
                    poly_token: Some(pair.poly_yes_token.to_string()),
                    pair_id: pair_id.clone(),
                    description: description.clone(),
                },
            ),
            (
                WsPlatform::Kalshi,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("kalshi-no"),
                    platform: WsPlatform::Kalshi,
                    action: OrderAction::Buy,
                    side: OutcomeSide::No,
                    price: req.no_price,
                    contracts,
                    kalshi_market_ticker: Some(pair.kalshi_market_ticker.to_string()),
                    poly_token: None,
                    pair_id,
                    description,
                },
            ),
        ],

        // Cross-platform: Kalshi YES + Poly NO
        ArbType::KalshiYesPolyNo => vec![
            (
                WsPlatform::Kalshi,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("kalshi-yes"),
                    platform: WsPlatform::Kalshi,
                    action: OrderAction::Buy,
                    side: OutcomeSide::Yes,
                    price: req.yes_price,
                    contracts,
                    kalshi_market_ticker: Some(pair.kalshi_market_ticker.to_string()),
                    poly_token: None,
                    pair_id: pair_id.clone(),
                    description: description.clone(),
                },
            ),
            (
                WsPlatform::Polymarket,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("poly-no"),
                    platform: WsPlatform::Polymarket,
                    action: OrderAction::Buy,
                    side: OutcomeSide::No,
                    price: req.no_price,
                    contracts,
                    kalshi_market_ticker: None,
                    poly_token: Some(pair.poly_no_token.to_string()),
                    pair_id,
                    description,
                },
            ),
        ],

        // Same-platform: Poly YES + Poly NO (both legs to the polymarket trader)
        ArbType::PolyOnly => vec![
            (
                WsPlatform::Polymarket,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("poly-yes"),
                    platform: WsPlatform::Polymarket,
                    action: OrderAction::Buy,
                    side: OutcomeSide::Yes,
                    price: req.yes_price,
                    contracts,
                    kalshi_market_ticker: None,
                    poly_token: Some(pair.poly_yes_token.to_string()),
                    pair_id: pair_id.clone(),
                    description: description.clone(),
                },
            ),
            (
                WsPlatform::Polymarket,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("poly-no"),
                    platform: WsPlatform::Polymarket,
                    action: OrderAction::Buy,
                    side: OutcomeSide::No,
                    price: req.no_price,
                    contracts,
                    kalshi_market_ticker: None,
                    poly_token: Some(pair.poly_no_token.to_string()),
                    pair_id,
                    description,
                },
            ),
        ],

        // Same-platform: Kalshi YES + Kalshi NO (both legs to the kalshi trader)
        ArbType::KalshiOnly => vec![
            (
                WsPlatform::Kalshi,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("kalshi-yes"),
                    platform: WsPlatform::Kalshi,
                    action: OrderAction::Buy,
                    side: OutcomeSide::Yes,
                    price: req.yes_price,
                    contracts,
                    kalshi_market_ticker: Some(pair.kalshi_market_ticker.to_string()),
                    poly_token: None,
                    pair_id: pair_id.clone(),
                    description: description.clone(),
                },
            ),
            (
                WsPlatform::Kalshi,
                IncomingMessage::ExecuteLeg {
                    market_id: req.market_id,
                    leg_id: mk_leg_id("kalshi-no"),
                    platform: WsPlatform::Kalshi,
                    action: OrderAction::Buy,
                    side: OutcomeSide::No,
                    price: req.no_price,
                    contracts,
                    kalshi_market_ticker: Some(pair.kalshi_market_ticker.to_string()),
                    poly_token: None,
                    pair_id,
                    description,
                },
            ),
        ],
    }
}

/// Remote execution event loop - forwards arbitrage opportunities to the remote trader.
pub async fn run_remote_execution_loop(
    mut rx: mpsc::Receiver<FastExecutionRequest>,
    executor: Arc<RemoteExecutor>,
) {
    info!(
        "[REMOTE_EXEC] Remote execution loop started (dry_run={})",
        executor.dry_run
    );

    while let Some(req) = rx.recv().await {
        let exec = executor.clone();
        tokio::spawn(async move {
            if let Err(e) = exec.process(req).await {
                error!("[REMOTE_EXEC] error: {}", e);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
    use crate::remote_protocol::{IncomingMessage as WsMsg, Platform as WsPlatform};
    use crate::remote_trader::RemoteTraderServer;
    use std::net::SocketAddr;

    fn make_test_pair() -> MarketPair {
        MarketPair {
            pair_id: "test-pair".into(),
            league: "epl".into(),
            market_type: crate::types::MarketType::Moneyline,
            description: "Test Market".into(),
            kalshi_event_ticker: "KXTEST".into(),
            kalshi_market_ticker: "KXTEST-MKT".into(),
            kalshi_event_slug: "test-market".into(),
            poly_slug: "test".into(),
            poly_yes_token: "0x_yes".into(),
            poly_no_token: "0x_no".into(),
            line_value: None,
            team_suffix: None,
        }
    }

    fn cb_disabled() -> Arc<CircuitBreaker> {
        Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            max_position_per_market: 1_000_000,
            max_total_position: 1_000_000,
            max_daily_loss: 1_000_000.0,
            max_consecutive_errors: 999,
            cooldown_secs: 0,
            enabled: false,
        }))
    }

    fn make_state_with_pair(pair: MarketPair) -> Arc<GlobalState> {
        let state = Arc::new(GlobalState::new());
        let id = state.add_pair(pair).expect("add_pair");
        assert_eq!(id, 0);
        state
    }

    #[tokio::test]
    async fn cross_platform_requires_both_traders() {
        let state = make_state_with_pair(make_test_pair());
        let cb = cb_disabled();

        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
        let router = server.router();

        let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;
        // No polymarket trader connected

        let exec = RemoteExecutor::new(state, cb, router, true);
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };
        exec.process(req).await.unwrap();

        assert!(kalshi_rx.try_recv().is_err(), "should not send when missing other platform");
    }

    #[tokio::test]
    async fn cross_platform_sends_one_leg_to_each_trader() {
        let state = make_state_with_pair(make_test_pair());
        let cb = cb_disabled();

        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
        let router = server.router();

        let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;
        let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

        let exec = RemoteExecutor::new(state, cb, router, true);
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };
        exec.process(req).await.unwrap();

        let kalshi_msg = kalshi_rx.try_recv().expect("kalshi leg");
        let poly_msg = poly_rx.try_recv().expect("poly leg");

        match kalshi_msg {
            WsMsg::ExecuteLeg { platform, kalshi_market_ticker, poly_token, .. } => {
                assert_eq!(platform, WsPlatform::Kalshi);
                assert!(kalshi_market_ticker.is_some());
                assert!(poly_token.is_none());
            }
            other => panic!("unexpected message to kalshi: {:?}", other),
        }

        match poly_msg {
            WsMsg::ExecuteLeg { platform, kalshi_market_ticker, poly_token, .. } => {
                assert_eq!(platform, WsPlatform::Polymarket);
                assert!(kalshi_market_ticker.is_none());
                assert!(poly_token.is_some());
            }
            other => panic!("unexpected message to poly: {:?}", other),
        }
    }

    #[tokio::test]
    async fn same_platform_can_send_two_legs_to_one_trader() {
        let state = make_state_with_pair(make_test_pair());
        let cb = cb_disabled();

        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = RemoteTraderServer::new(bind, vec![WsPlatform::Polymarket], true);
        let router = server.router();

        let mut poly_rx = router.test_register(WsPlatform::Polymarket).await;

        let exec = RemoteExecutor::new(state, cb, router, true);
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 48,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyOnly,
            detected_ns: 0,
        };
        exec.process(req).await.unwrap();

        let m1 = poly_rx.try_recv().expect("first poly leg");
        let m2 = poly_rx.try_recv().expect("second poly leg");

        assert!(matches!(m1, WsMsg::ExecuteLeg { platform: WsPlatform::Polymarket, .. }));
        assert!(matches!(m2, WsMsg::ExecuteLeg { platform: WsPlatform::Polymarket, .. }));
    }
}

