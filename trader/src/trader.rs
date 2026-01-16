//! Main trader logic and state machine

use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::protocol::{ArbType, IncomingMessage, OrderAction, OutcomeSide, OutgoingMessage, Platform};

use crate::api::kalshi::{KalshiApiClient, KalshiConfig};
use crate::api::polymarket::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};

const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
const POLYGON_CHAIN_ID: u64 = 137;

/// Trader state
pub enum TraderState {
    Uninitialized,
    Initialized {
        platform: Platform,
        kalshi: Option<Arc<KalshiApiClient>>,
        poly: Option<Arc<SharedAsyncClient>>,
        dry_run: bool,
    },
    Error {
        message: String,
    },
}

/// Main trader that processes commands and manages execution
pub struct Trader {
    state: TraderState,
    config: Arc<Config>,
}

impl Trader {
    /// Create a new trader
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            state: TraderState::Uninitialized,
            config,
        }
    }

    /// Handle an incoming message and return response
    pub async fn handle_message(&mut self, msg: IncomingMessage) -> OutgoingMessage {
        match msg {
            IncomingMessage::Init { platforms, dry_run } => {
                self.handle_init(platforms, dry_run).await
            }
            IncomingMessage::ExecuteLeg {
                market_id,
                leg_id,
                platform,
                action,
                side,
                price,
                contracts,
                kalshi_market_ticker,
                poly_token,
                pair_id,
                description,
            } => {
                self.handle_execute_leg(
                    market_id,
                    leg_id,
                    platform,
                    action,
                    side,
                    price,
                    contracts,
                    kalshi_market_ticker,
                    poly_token,
                    pair_id,
                    description,
                )
                .await
            }
            IncomingMessage::Execute {
                market_id,
                arb_type,
                yes_price,
                no_price,
                yes_size,
                no_size,
                pair_id,
                description,
                kalshi_market_ticker,
                poly_yes_token,
                poly_no_token,
            } => {
                self.handle_execute(
                    market_id,
                    arb_type,
                    yes_price,
                    no_price,
                    yes_size,
                    no_size,
                    pair_id,
                    description,
                    kalshi_market_ticker,
                    poly_yes_token,
                    poly_no_token,
                )
                .await
            }
            IncomingMessage::Ping { timestamp } => self.handle_ping(timestamp),
            IncomingMessage::Pong { timestamp } => OutgoingMessage::Pong { timestamp },
            IncomingMessage::Status => self.handle_status(),
        }
    }

    /// Handle initialization request
    async fn handle_init(
        &mut self,
        platforms: Vec<Platform>,
        dry_run: bool,
    ) -> OutgoingMessage {
        info!(
            "[TRADER] Received init request: platforms={:?}, dry_run={} (trader_platform={:?})",
            platforms, dry_run, self.config.platform
        );

        // The trader must only ever execute for its configured platform.
        let platform = self.config.platform;
        if !platforms.iter().any(|p| *p == platform) {
            let msg = format!(
                "Controller did not request our platform {:?} (requested={:?})",
                platform, platforms
            );
            warn!("[TRADER] {}", msg);
            self.state = TraderState::Error { message: msg.clone() };
            return OutgoingMessage::InitAck {
                success: false,
                platforms: vec![],
                error: Some(msg),
            };
        }

        let effective_dry_run = dry_run || self.config.dry_run;

        let init_res =
            (|| -> anyhow::Result<(Option<Arc<KalshiApiClient>>, Option<Arc<SharedAsyncClient>>)> {
                match platform {
                    Platform::Kalshi => {
                        if effective_dry_run {
                            return Ok((None, None));
                        }
                        let api_key = self
                            .config
                            .kalshi_api_key
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Missing KALSHI_API_KEY"))?;
                        let private_key = self
                            .config
                            .kalshi_private_key
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Missing KALSHI_PRIVATE_KEY"))?;
                        let kalshi_cfg = KalshiConfig::new(api_key, &private_key)?;
                        let kalshi = Arc::new(KalshiApiClient::new(kalshi_cfg));
                        Ok((Some(kalshi), None))
                    }
                    Platform::Polymarket => {
                        if effective_dry_run {
                            return Ok((None, None));
                        }
                        let private_key = self
                            .config
                            .polymarket_private_key
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Missing POLYMARKET_PRIVATE_KEY"))?;
                        let funder = self
                            .config
                            .polymarket_funder
                            .clone()
                            .ok_or_else(|| {
                                anyhow::anyhow!("Missing POLYMARKET_FUNDER (or POLY_FUNDER)")
                            })?;
                        let api_key = self
                            .config
                            .polymarket_api_key
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Missing POLYMARKET_API_KEY"))?;
                        let api_secret = self
                            .config
                            .polymarket_api_secret
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Missing POLYMARKET_API_SECRET"))?;

                        let poly_async = PolymarketAsyncClient::new(
                            POLY_CLOB_HOST,
                            POLYGON_CHAIN_ID,
                            &private_key,
                            &funder,
                        )?;
                        let creds = PreparedCreds::new(api_key, api_secret);
                        let shared =
                            Arc::new(SharedAsyncClient::new(poly_async, creds, POLYGON_CHAIN_ID));
                        Ok((None, Some(shared)))
                    }
                }
            })();

        match init_res {
            Ok((kalshi, poly)) => {
                self.state = TraderState::Initialized {
                    platform,
                    kalshi,
                    poly,
                    dry_run: effective_dry_run,
                };
                info!("[TRADER] Initialized successfully for platform {:?}", platform);
                OutgoingMessage::InitAck {
                    success: true,
                    platforms: vec![platform],
                    error: None,
                }
            }
            Err(e) => {
                error!("[TRADER] Initialization failed: {}", e);
                self.state = TraderState::Error {
                    message: e.to_string(),
                };
                OutgoingMessage::InitAck {
                    success: false,
                    platforms: vec![],
                    error: Some(e.to_string()),
                }
            }
        }
    }

    async fn handle_execute_leg(
        &mut self,
        market_id: u16,
        leg_id: String,
        platform: Platform,
        action: OrderAction,
        side: OutcomeSide,
        price: u16,
        contracts: i64,
        kalshi_market_ticker: Option<String>,
        poly_token: Option<String>,
        pair_id: Option<String>,
        description: Option<String>,
    ) -> OutgoingMessage {
        let start = Instant::now();

        // Log helpful metadata (also avoids unused-variable warnings in future changes)
        debug!(
            "[TRADER] execute_leg: market_id={} leg_id={} platform={:?} action={:?} side={:?} price={} contracts={} pair_id={:?} desc={:?}",
            market_id, leg_id, platform, action, side, price, contracts, pair_id.as_deref(), description.as_deref()
        );

        match &self.state {
            TraderState::Uninitialized => {
                return OutgoingMessage::LegResult {
                    market_id,
                    leg_id,
                    platform,
                    success: false,
                    latency_ns: start.elapsed().as_nanos() as u64,
                    error: Some("Trader not initialized".to_string()),
                };
            }
            TraderState::Error { message } => {
                return OutgoingMessage::LegResult {
                    market_id,
                    leg_id,
                    platform,
                    success: false,
                    latency_ns: start.elapsed().as_nanos() as u64,
                    error: Some(format!("Trader error: {}", message)),
                };
            }
            TraderState::Initialized { platform: cfg_platform, dry_run, kalshi, poly } => {
                if *cfg_platform != platform {
                    return OutgoingMessage::LegResult {
                        market_id,
                        leg_id,
                        platform,
                        success: false,
                        latency_ns: start.elapsed().as_nanos() as u64,
                        error: Some(format!("Trader is configured for {:?} only", cfg_platform)),
                    };
                }

                // Execute the single leg on our platform.
                let res: anyhow::Result<()> = async {
                    match platform {
                        Platform::Kalshi => {
                            let ticker = kalshi_market_ticker
                                .as_deref()
                                .ok_or_else(|| anyhow::anyhow!("Missing kalshi_market_ticker"))?;
                            let side_str = match side {
                                OutcomeSide::Yes => "yes",
                                OutcomeSide::No => "no",
                            };
                            if *dry_run {
                                info!(
                                    "[KALSHI] DRY RUN leg_id={} {} {} {}x @ {}¢",
                                    leg_id,
                                    match action {
                                        OrderAction::Buy => "buy",
                                        OrderAction::Sell => "sell",
                                    },
                                    side_str,
                                    contracts,
                                    price
                                );
                                Ok(())
                            } else {
                                let client = kalshi
                                    .as_ref()
                                    .ok_or_else(|| anyhow::anyhow!("Kalshi client not initialized"))?;
                                match action {
                                    OrderAction::Buy => {
                                        let resp = client
                                            .buy_ioc(ticker, side_str, price as i64, contracts)
                                            .await?;
                                        info!(
                                            "[KALSHI] leg_id={} order_id={} filled={} cost={}¢",
                                            leg_id,
                                            resp.order.order_id,
                                            resp.order.filled_count(),
                                            resp.order.taker_fill_cost.unwrap_or(0)
                                                + resp.order.maker_fill_cost.unwrap_or(0)
                                        );
                                    }
                                    OrderAction::Sell => {
                                        let resp = client
                                            .sell_ioc(ticker, side_str, price as i64, contracts)
                                            .await?;
                                        info!(
                                            "[KALSHI] leg_id={} order_id={} filled={} cost={}¢",
                                            leg_id,
                                            resp.order.order_id,
                                            resp.order.filled_count(),
                                            resp.order.taker_fill_cost.unwrap_or(0)
                                                + resp.order.maker_fill_cost.unwrap_or(0)
                                        );
                                    }
                                }
                                Ok(())
                            }
                        }
                        Platform::Polymarket => {
                            let token = poly_token
                                .as_deref()
                                .ok_or_else(|| anyhow::anyhow!("Missing poly_token"))?;
                            let p = (price as f64) / 100.0;
                            if *dry_run {
                                info!(
                                    "[POLY] DRY RUN leg_id={} {:?} {:?} {}x @ {:.4}",
                                    leg_id, action, side, contracts, p
                                );
                                Ok(())
                            } else {
                                let client = poly
                                    .as_ref()
                                    .ok_or_else(|| anyhow::anyhow!("Polymarket client not initialized"))?;
                                match action {
                                    OrderAction::Buy => {
                                        let fill = client.buy_fak(token, p, contracts as f64).await?;
                                        info!(
                                            "[POLY] leg_id={} order_id={} filled={:.2} cost={:.4}",
                                            leg_id, fill.order_id, fill.filled_size, fill.fill_cost
                                        );
                                    }
                                    OrderAction::Sell => {
                                        let fill = client.sell_fak(token, p, contracts as f64).await?;
                                        info!(
                                            "[POLY] leg_id={} order_id={} filled={:.2} cost={:.4}",
                                            leg_id, fill.order_id, fill.filled_size, fill.fill_cost
                                        );
                                    }
                                }
                                Ok(())
                            }
                        }
                    }
                }
                .await;

                match res {
                    Ok(()) => OutgoingMessage::LegResult {
                        market_id,
                        leg_id,
                        platform,
                        success: true,
                        latency_ns: start.elapsed().as_nanos() as u64,
                        error: None,
                    },
                    Err(e) => OutgoingMessage::LegResult {
                        market_id,
                        leg_id,
                        platform,
                        success: false,
                        latency_ns: start.elapsed().as_nanos() as u64,
                        error: Some(e.to_string()),
                    },
                }
            }
        }
    }

    /// Handle order execution request
    async fn handle_execute(
        &mut self,
        market_id: u16,
        arb_type: ArbType,
        yes_price: u16,
        no_price: u16,
        yes_size: u16,
        no_size: u16,
        pair_id: Option<String>,
        description: Option<String>,
        kalshi_market_ticker: Option<String>,
        poly_yes_token: Option<String>,
        poly_no_token: Option<String>,
    ) -> OutgoingMessage {
        // Legacy `execute` support: convert to one or two legs, and execute only for our platform.
        let start = Instant::now();

        let max_contracts = (yes_size.min(no_size) / 100) as i64;
        if max_contracts < 1 {
            return OutgoingMessage::ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: start.elapsed().as_nanos() as u64,
                error: Some("Insufficient liquidity".to_string()),
            };
        }

        let legs = self.legacy_to_legs(
            market_id,
            arb_type,
            yes_price,
            no_price,
            max_contracts,
            kalshi_market_ticker.clone(),
            poly_yes_token.clone(),
            poly_no_token.clone(),
            pair_id.clone(),
            description.clone(),
        );

        if legs.is_empty() {
            return OutgoingMessage::ExecutionResult {
                market_id,
                success: false,
                profit_cents: 0,
                latency_ns: start.elapsed().as_nanos() as u64,
                error: Some("No executable legs for this trader".to_string()),
            };
        }

        // Execute sequentially; aggregate result.
        for (leg_id, platform, action, side, price, contracts, kalshi_ticker, poly_token) in legs {
            let res = self
                .handle_execute_leg(
                    market_id,
                    leg_id,
                    platform,
                    action,
                    side,
                    price,
                    contracts,
                    kalshi_ticker,
                    poly_token,
                    pair_id.clone(),
                    description.clone(),
                )
                .await;
            if let OutgoingMessage::LegResult { success: false, error, .. } = res {
                return OutgoingMessage::ExecutionResult {
                    market_id,
                    success: false,
                    profit_cents: 0,
                    latency_ns: start.elapsed().as_nanos() as u64,
                    error,
                };
            }
        }

        OutgoingMessage::ExecutionResult {
            market_id,
            success: true,
            profit_cents: 0,
            latency_ns: start.elapsed().as_nanos() as u64,
            error: Some("LEGACY_EXECUTE".to_string()),
        }
    }

    /// Handle ping message
    fn handle_ping(&self, timestamp: u64) -> OutgoingMessage {
        OutgoingMessage::Pong { timestamp }
    }

    /// Handle status request
    fn handle_status(&self) -> OutgoingMessage {
        match &self.state {
            TraderState::Uninitialized => OutgoingMessage::Status {
                connected: true,
                platforms: vec![],
                dry_run: self.config.dry_run,
            },
            TraderState::Initialized {
                platform,
                dry_run,
                ..
            } => OutgoingMessage::Status {
                connected: true,
                platforms: vec![*platform],
                dry_run: *dry_run,
            },
            TraderState::Error { .. } => OutgoingMessage::Status {
                connected: true,
                platforms: vec![],
                dry_run: self.config.dry_run,
            },
        }
    }

    /// Get current state
    #[allow(dead_code)] // Used by some integrations/tests
    pub fn state(&self) -> &TraderState {
        &self.state
    }
}

impl Trader {
    /// Convert legacy Execute messages into single-leg instructions for this trader.
    /// Returns tuples: (leg_id, platform, action, side, price, contracts, kalshi_ticker, poly_token)
    fn legacy_to_legs(
        &self,
        market_id: u16,
        arb_type: ArbType,
        yes_price: u16,
        no_price: u16,
        contracts: i64,
        kalshi_market_ticker: Option<String>,
        poly_yes_token: Option<String>,
        poly_no_token: Option<String>,
        _pair_id: Option<String>,
        _description: Option<String>,
    ) -> Vec<(String, Platform, OrderAction, OutcomeSide, u16, i64, Option<String>, Option<String>)> {
        let platform = self.config.platform;
        let mk = |suffix: &str| format!("legacy-m{}-{}", market_id, suffix);

        match arb_type {
            ArbType::PolyYesKalshiNo => match platform {
                Platform::Polymarket => vec![(
                    mk("poly-yes"),
                    Platform::Polymarket,
                    OrderAction::Buy,
                    OutcomeSide::Yes,
                    yes_price,
                    contracts,
                    None,
                    poly_yes_token,
                )],
                Platform::Kalshi => vec![(
                    mk("kalshi-no"),
                    Platform::Kalshi,
                    OrderAction::Buy,
                    OutcomeSide::No,
                    no_price,
                    contracts,
                    kalshi_market_ticker,
                    None,
                )],
            },
            ArbType::KalshiYesPolyNo => match platform {
                Platform::Kalshi => vec![(
                    mk("kalshi-yes"),
                    Platform::Kalshi,
                    OrderAction::Buy,
                    OutcomeSide::Yes,
                    yes_price,
                    contracts,
                    kalshi_market_ticker,
                    None,
                )],
                Platform::Polymarket => vec![(
                    mk("poly-no"),
                    Platform::Polymarket,
                    OrderAction::Buy,
                    OutcomeSide::No,
                    no_price,
                    contracts,
                    None,
                    poly_no_token,
                )],
            },
            ArbType::PolyOnly => {
                if platform != Platform::Polymarket {
                    return vec![];
                }
                vec![
                    (
                        mk("poly-yes"),
                        Platform::Polymarket,
                        OrderAction::Buy,
                        OutcomeSide::Yes,
                        yes_price,
                        contracts,
                        None,
                        poly_yes_token,
                    ),
                    (
                        mk("poly-no"),
                        Platform::Polymarket,
                        OrderAction::Buy,
                        OutcomeSide::No,
                        no_price,
                        contracts,
                        None,
                        poly_no_token,
                    ),
                ]
            }
            ArbType::KalshiOnly => {
                if platform != Platform::Kalshi {
                    return vec![];
                }
                vec![
                    (
                        mk("kalshi-yes"),
                        Platform::Kalshi,
                        OrderAction::Buy,
                        OutcomeSide::Yes,
                        yes_price,
                        contracts,
                        kalshi_market_ticker.clone(),
                        None,
                    ),
                    (
                        mk("kalshi-no"),
                        Platform::Kalshi,
                        OrderAction::Buy,
                        OutcomeSide::No,
                        no_price,
                        contracts,
                        kalshi_market_ticker,
                        None,
                    ),
                ]
            }
        }
    }
}
