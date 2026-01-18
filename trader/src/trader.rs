//! Main trader logic and state machine

use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::protocol::{
    ArbType, IncomingMessage, OrderAction, OutcomeSide, OutgoingMessage, Platform,
};

// Use trading crate for execution
use trading::execution::{
    execute_leg, ExecutionClients, LegRequest,
    OrderAction as TradingOrderAction, OutcomeSide as TradingOutcomeSide,
    Platform as TradingPlatform,
};
use trading::kalshi::KalshiConfig;
use trading::kalshi::KalshiApiClient;
use trading::polymarket::{ApiCreds, PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};

// RSA for parsing private keys
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::RsaPrivateKey;

const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
const POLYGON_CHAIN_ID: u64 = 137;

/// Type alias for legacy leg vector to reduce complexity
type LegacyLegVec = Vec<(String, Platform, OrderAction, OutcomeSide, u16, i64, Option<String>, Option<String>)>;

/// Trader state
pub enum TraderState {
    Uninitialized,
    Initialized {
        platform: Platform,
        clients: ExecutionClients,
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
        if !platforms.contains(&platform) {
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

        let init_res = self.init_clients(platform, effective_dry_run).await;

        match init_res {
            Ok(clients) => {
                self.state = TraderState::Initialized {
                    platform,
                    clients,
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

    /// Initialize execution clients based on platform
    async fn init_clients(
        &self,
        platform: Platform,
        dry_run: bool,
    ) -> anyhow::Result<ExecutionClients> {
        match platform {
            Platform::Kalshi => {
                if dry_run {
                    return Ok(ExecutionClients {
                        kalshi: None,
                        polymarket: None,
                    });
                }
                let api_key = self
                    .config
                    .kalshi_api_key
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Missing KALSHI_API_KEY"))?;
                let private_key_pem = self
                    .config
                    .kalshi_private_key
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Missing KALSHI_PRIVATE_KEY"))?;

                // Parse the private key PEM
                let private_key = RsaPrivateKey::from_pkcs1_pem(private_key_pem.trim())
                    .map_err(|e| anyhow::anyhow!("Failed to parse Kalshi private key: {}", e))?;

                let kalshi_cfg = KalshiConfig {
                    api_key_id: api_key,
                    private_key,
                };
                let kalshi = Arc::new(KalshiApiClient::new(kalshi_cfg));

                Ok(ExecutionClients {
                    kalshi: Some(kalshi),
                    polymarket: None,
                })
            }
            Platform::Polymarket => {
                if dry_run {
                    return Ok(ExecutionClients {
                        kalshi: None,
                        polymarket: None,
                    });
                }
                let private_key = self
                    .config
                    .polymarket_private_key
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Missing POLY_PRIVATE_KEY"))?;
                let funder = self
                    .config
                    .polymarket_funder
                    .clone()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing POLYMARKET_FUNDER (or POLY_FUNDER)")
                    })?;

                let poly_async = PolymarketAsyncClient::new_with_signature_type(
                    POLY_CLOB_HOST,
                    POLYGON_CHAIN_ID,
                    &private_key,
                    &funder,
                    self.config.polymarket_signature_type,
                )?;

                // Prefer explicit API creds if provided, otherwise derive them from the wallet.
                // This matches the "it should figure it out" expectation and avoids extra setup.
                let api_creds = match (
                    self.config.polymarket_api_key.clone(),
                    self.config.polymarket_api_secret.clone(),
                    self.config.polymarket_api_passphrase.clone(),
                ) {
                    (Some(api_key), Some(api_secret), Some(api_passphrase)) => ApiCreds {
                        api_key,
                        api_secret,
                        api_passphrase,
                    },
                    _ => {
                        // Polymarket's derive-api-key flow is sensitive; use the canonical nonce of 0.
                        // (Using a changing nonce can trigger "Could not derive api key!" on some accounts.)
                        let nonce = 0_u64;
                        info!(
                            "[TRADER] Deriving Polymarket API credentials via /auth/derive-api-key (address={}, funder={}, nonce={})...",
                            poly_async.wallet_address(),
                            funder,
                            nonce
                        );
                        poly_async.derive_api_key(nonce).await?
                    }
                };
                let creds = PreparedCreds::from_api_creds(&api_creds)?;
                let shared = Arc::new(SharedAsyncClient::new(poly_async, creds, POLYGON_CHAIN_ID));

                Ok(ExecutionClients {
                    kalshi: None,
                    polymarket: Some(shared),
                })
            }
        }
    }

    /// Handle execute leg request using the trading crate's execute_leg function
    #[allow(clippy::too_many_arguments)]
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

        // Log helpful metadata
        debug!(
            "[TRADER] execute_leg: market_id={} leg_id={} platform={:?} action={:?} side={:?} price={} contracts={} pair_id={:?} desc={:?}",
            market_id, leg_id, platform, action, side, price, contracts, pair_id.as_deref(), description.as_deref()
        );

        match &self.state {
            TraderState::Uninitialized => {
                OutgoingMessage::LegResult {
                    market_id,
                    leg_id,
                    platform,
                    action,
                    side,
                    price,
                    contracts,
                    success: false,
                    latency_ns: start.elapsed().as_nanos() as u64,
                    error: Some("Trader not initialized".to_string()),
                }
            }
            TraderState::Error { message } => {
                OutgoingMessage::LegResult {
                    market_id,
                    leg_id,
                    platform,
                    action,
                    side,
                    price,
                    contracts,
                    success: false,
                    latency_ns: start.elapsed().as_nanos() as u64,
                    error: Some(format!("Trader error: {}", message)),
                }
            }
            TraderState::Initialized {
                platform: cfg_platform,
                dry_run,
                clients,
            } => {
                if *cfg_platform != platform {
                    return OutgoingMessage::LegResult {
                        market_id,
                        leg_id,
                        platform,
                        action,
                        side,
                        price,
                        contracts,
                        success: false,
                        latency_ns: start.elapsed().as_nanos() as u64,
                        error: Some(format!("Trader is configured for {:?} only", cfg_platform)),
                    };
                }

                // Convert protocol types to trading crate types
                let trading_platform = convert_platform(platform);
                let trading_action = convert_action(action);
                let trading_side = convert_side(side);

                // Build the leg request
                let leg_request = LegRequest {
                    leg_id: &leg_id,
                    platform: trading_platform,
                    action: trading_action,
                    side: trading_side,
                    price_cents: price,
                    contracts,
                    kalshi_ticker: kalshi_market_ticker.as_deref(),
                    poly_token: poly_token.as_deref(),
                };

                // Execute using the shared execute_leg function
                let result = execute_leg(&leg_request, clients, *dry_run).await;

                OutgoingMessage::LegResult {
                    market_id,
                    leg_id,
                    platform,
                    action,
                    side,
                    price,
                    contracts,
                    success: result.success,
                    latency_ns: result.latency_ns,
                    error: result.error,
                }
            }
        }
    }

    /// Handle order execution request (legacy)
    #[allow(clippy::too_many_arguments)]
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
    #[allow(clippy::too_many_arguments)]
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
    ) -> LegacyLegVec
    {
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

// Conversion functions from protocol types to trading crate types
fn convert_platform(p: Platform) -> TradingPlatform {
    match p {
        Platform::Kalshi => TradingPlatform::Kalshi,
        Platform::Polymarket => TradingPlatform::Polymarket,
    }
}

fn convert_action(a: OrderAction) -> TradingOrderAction {
    match a {
        OrderAction::Buy => TradingOrderAction::Buy,
        OrderAction::Sell => TradingOrderAction::Sell,
    }
}

fn convert_side(s: OutcomeSide) -> TradingOutcomeSide {
    match s {
        OutcomeSide::Yes => TradingOutcomeSide::Yes,
        OutcomeSide::No => TradingOutcomeSide::No,
    }
}
