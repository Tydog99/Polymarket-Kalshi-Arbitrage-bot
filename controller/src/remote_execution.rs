//! Execution path that sends trades to a remote trader over WebSocket,
//! with optional local execution for authorized platforms.

use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::circuit_breaker::CircuitBreaker;
use crate::config::{KALSHI_WEB_BASE, build_polymarket_url};
use crate::execution::describe_arb_trade;
use crate::remote_protocol::{IncomingMessage, OrderAction, OutcomeSide, Platform as WsPlatform};
use crate::remote_trader::RemoteTraderRouter;
use crate::types::{ArbType, FastExecutionRequest, GlobalState, MarketPair};

use trading::execution::{
    execute_leg, ExecutionClients, LegRequest, OrderAction as TradingOrderAction,
    OutcomeSide as TradingOutcomeSide, Platform as TradingPlatform,
};
use trading::kalshi::KalshiApiClient;
use trading::polymarket::SharedAsyncClient;

pub struct HybridExecutor {
    state: Arc<GlobalState>,
    circuit_breaker: Arc<CircuitBreaker>,
    router: RemoteTraderRouter,
    local_platforms: HashSet<TradingPlatform>,
    local_clients: ExecutionClients,
    in_flight: Arc<[AtomicU64; 8]>,
    leg_seq: AtomicU64,
    pub dry_run: bool,
}

/// Convert WsPlatform to TradingPlatform.
fn ws_to_trading_platform(p: WsPlatform) -> TradingPlatform {
    match p {
        WsPlatform::Kalshi => TradingPlatform::Kalshi,
        WsPlatform::Polymarket => TradingPlatform::Polymarket,
    }
}

impl HybridExecutor {
    pub fn new(
        state: Arc<GlobalState>,
        circuit_breaker: Arc<CircuitBreaker>,
        router: RemoteTraderRouter,
        local_platforms: HashSet<TradingPlatform>,
        kalshi_api: Option<Arc<KalshiApiClient>>,
        poly_async: Option<Arc<SharedAsyncClient>>,
        dry_run: bool,
    ) -> Self {
        Self {
            state,
            circuit_breaker,
            router,
            local_platforms,
            local_clients: ExecutionClients {
                kalshi: kalshi_api,
                polymarket: poly_async,
            },
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

        // Describe what tokens are being purchased
        let trade_description = describe_arb_trade(
            req.arb_type,
            &pair.kalshi_market_ticker,
            &pair.poly_yes_token,
            &pair.poly_no_token,
            pair.team_suffix.as_deref(),
        );

        info!(
            "[HYBRID] arb detected: {} | {} | {:?} y={}c n={}c | profit={}c | {}x",
            pair.description, trade_description, req.arb_type, req.yes_price, req.no_price, profit_cents, max_contracts
        );

        // Build Kalshi URL: https://kalshi.com/markets/{series}/{slug}/{event_ticker}
        let kalshi_series = pair.kalshi_event_ticker
            .split('-')
            .next()
            .unwrap_or(&pair.kalshi_event_ticker)
            .to_lowercase();
        let kalshi_event_ticker_lower = pair.kalshi_event_ticker.to_lowercase();
        let poly_url = build_polymarket_url(&pair.league, &pair.poly_slug);
        info!(
            "[REMOTE_EXEC] 🔗 Kalshi: {}/{}/{}/{} | Polymarket: {}",
            KALSHI_WEB_BASE,
            kalshi_series,
            pair.kalshi_event_slug,
            kalshi_event_ticker_lower,
            poly_url
        );

        if self.dry_run {
            info!(
                "[HYBRID] DRY RUN - executing legs (no real orders)"
            );
        }

        // Build legs for this arb
        let legs = build_legs(&req, &pair, max_contracts, &self.leg_seq);
        if legs.is_empty() {
            self.release_in_flight_delayed(market_id);
            return Ok(());
        }

        // Phase 1: Verify all legs can execute (either remote or local)
        for (ws_platform, _) in &legs {
            let trading_platform = ws_to_trading_platform(*ws_platform);
            let has_remote = self.router.is_connected(*ws_platform).await;
            let has_local = self.local_platforms.contains(&trading_platform);

            if !has_remote && !has_local {
                warn!(
                    "[HYBRID] Cannot execute {:?} leg - no remote trader and not authorized locally; dropping arb market_id={}",
                    trading_platform, market_id
                );
                self.release_in_flight_delayed(market_id);
                return Ok(());
            }
        }

        // Phase 2: Execute all legs (prefer remote, fallback to local)
        for (ws_platform, msg) in legs {
            if self.router.is_connected(ws_platform).await {
                if !self.router.try_send(ws_platform, msg).await {
                    warn!(
                        "[HYBRID] Failed to send leg to remote {:?}; market_id={}",
                        ws_platform, market_id
                    );
                }
            } else if let Err(e) = self.execute_local_leg(ws_platform, msg).await {
                warn!("[HYBRID] Local execution failed: {}", e);
            }
        }

        self.release_in_flight_delayed(market_id);
        Ok(())
    }

    async fn execute_local_leg(&self, ws_platform: WsPlatform, msg: IncomingMessage) -> Result<()> {
        let IncomingMessage::ExecuteLeg {
            market_id: _,
            leg_id,
            platform: _,
            action,
            side,
            price,
            contracts,
            kalshi_market_ticker,
            poly_token,
            pair_id: _,
            description: _,
        } = msg
        else {
            return Ok(());
        };

        let trading_platform = ws_to_trading_platform(ws_platform);

        let leg_action = match action {
            OrderAction::Buy => TradingOrderAction::Buy,
            OrderAction::Sell => TradingOrderAction::Sell,
        };
        let leg_side = match side {
            OutcomeSide::Yes => TradingOutcomeSide::Yes,
            OutcomeSide::No => TradingOutcomeSide::No,
        };

        let req = LegRequest {
            leg_id: &leg_id,
            platform: trading_platform,
            action: leg_action,
            side: leg_side,
            price_cents: price,
            contracts,
            kalshi_ticker: kalshi_market_ticker.as_deref(),
            poly_token: poly_token.as_deref(),
        };

        let result = execute_leg(&req, &self.local_clients, self.dry_run).await;

        if result.success {
            info!(
                "[LOCAL] ✅ {:?} {:?} {:?} {}x @ {}¢ leg_id={} latency={}µs",
                trading_platform, action, side, contracts, price, leg_id, result.latency_ns / 1000
            );
            Ok(())
        } else {
            warn!(
                "[LOCAL] ❌ {:?} {:?} {:?} {}x @ {}¢ leg_id={} err={}",
                trading_platform, action, side, contracts, price, leg_id,
                result.error.as_deref().unwrap_or("unknown")
            );
            Err(anyhow!(result.error.unwrap_or_else(|| "unknown".into())))
        }
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

/// Hybrid execution event loop - forwards arbitrage opportunities to remote traders or executes locally.
pub async fn run_hybrid_execution_loop(
    mut rx: mpsc::Receiver<FastExecutionRequest>,
    executor: Arc<HybridExecutor>,
) {
    info!(
        "[HYBRID] Hybrid execution loop started (dry_run={})",
        executor.dry_run
    );

    while let Some(req) = rx.recv().await {
        let exec = executor.clone();
        tokio::spawn(async move {
            if let Err(e) = exec.process(req).await {
                error!("[HYBRID] error: {}", e);
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
    async fn cross_platform_requires_both_traders_or_local() {
        let state = make_state_with_pair(make_test_pair());
        let cb = cb_disabled();

        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = RemoteTraderServer::new(bind, vec![WsPlatform::Kalshi, WsPlatform::Polymarket], true);
        let router = server.router();

        let mut kalshi_rx = router.test_register(WsPlatform::Kalshi).await;
        // No polymarket trader connected and no local authorization

        let exec = HybridExecutor::new(state, cb, router, HashSet::new(), None, None, true);
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

        let exec = HybridExecutor::new(state, cb, router, HashSet::new(), None, None, true);
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

        let exec = HybridExecutor::new(state, cb, router, HashSet::new(), None, None, true);
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

