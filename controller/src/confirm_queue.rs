//! Confirmation queue for pending arbitrage opportunities.
//!
//! Manages a keyed queue of arbs awaiting user confirmation. One slot per market,
//! new arbs for existing markets replace old data to keep prices fresh.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use crate::arb::{kalshi_fee, ArbConfig, ArbOpportunity};
use crate::config::{build_polymarket_url, KALSHI_WEB_BASE};
use crate::types::{ArbType, FastExecutionRequest, GlobalState, MarketPair};

/// A pending arbitrage opportunity awaiting confirmation
#[derive(Debug, Clone)]
pub struct PendingArb {
    /// Original execution request
    pub request: FastExecutionRequest,
    /// Market pair info (for display)
    pub pair: Arc<MarketPair>,
    /// Number of times this arb has been detected while pending
    pub detection_count: u32,
    /// Kalshi market URL
    pub kalshi_url: String,
    /// Polymarket event URL
    pub poly_url: String,
}

impl PendingArb {
    pub fn new(request: FastExecutionRequest, pair: Arc<MarketPair>) -> Self {
        let kalshi_url = format!("{}/{}", KALSHI_WEB_BASE, pair.kalshi_market_ticker);
        let poly_url = build_polymarket_url(&pair.league, &pair.poly_slug);

        Self {
            request,
            pair,
            detection_count: 1,
            kalshi_url,
            poly_url,
        }
    }

    /// Update with fresher price data
    pub fn update(&mut self, request: FastExecutionRequest) {
        self.request = request;
        self.detection_count += 1;
    }

    /// Calculate profit in cents based on current request prices
    pub fn profit_cents(&self) -> i16 {
        self.request.profit_cents()
    }

    /// Calculate max contracts based on available size and prices.
    /// Returns the minimum of (yes_size / yes_price) and (no_size / no_price).
    pub fn max_contracts(&self) -> i64 {
        if self.request.yes_price == 0 || self.request.no_price == 0 {
            return 0;
        }
        let yes_contracts = self.request.yes_size / self.request.yes_price;
        let no_contracts = self.request.no_size / self.request.no_price;
        yes_contracts.min(no_contracts) as i64
    }
}

/// User action on a pending arb
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Approve execution
    Proceed,
    /// Reject (can re-queue on next detection)
    Reject { note: Option<String> },
    /// Reject and blacklist for 5 minutes
    Blacklist { note: Option<String> },
}

/// Blacklist entry for temporarily suppressed markets
struct BlacklistEntry {
    expires: Instant,
}

/// Validation result with details about price movement
pub struct ValidationResult {
    pub is_valid: bool,
    pub original_cost: u16,
    pub current_cost: u16,
}

/// Queue of pending arbitrage opportunities keyed by market_id
pub struct ConfirmationQueue {
    /// Global state for price lookups
    state: Arc<GlobalState>,
    /// Pending arbs keyed by market_id (one slot per market)
    pending: RwLock<HashMap<u16, PendingArb>>,
    /// Order of market_ids for display (first-in order)
    order: RwLock<VecDeque<u16>>,
    /// Blacklisted markets (market_id -> expiry)
    blacklist: RwLock<HashMap<u16, BlacklistEntry>>,
    /// Channel to notify TUI of updates
    update_tx: mpsc::Sender<()>,
}

impl ConfirmationQueue {
    pub fn new(state: Arc<GlobalState>, update_tx: mpsc::Sender<()>) -> Self {
        Self {
            state,
            pending: RwLock::new(HashMap::new()),
            order: RwLock::new(VecDeque::new()),
            blacklist: RwLock::new(HashMap::new()),
            update_tx,
        }
    }

    /// Push a new arb opportunity (or update existing for same market).
    /// Returns `true` if this was a new entry, `false` if updating existing or blacklisted.
    /// Note: Size validation is done upstream in ArbOpportunity::new()
    pub async fn push(&self, request: FastExecutionRequest, pair: Arc<MarketPair>) -> bool {
        let market_id = request.market_id;

        // Check blacklist and clean expired entries in one lock acquisition
        {
            let mut blacklist = self.blacklist.write().await;
            let now = Instant::now();

            // Check if this market is blacklisted
            if let Some(entry) = blacklist.get(&market_id) {
                if entry.expires > now {
                    return false; // Still blacklisted
                }
            }

            // Clean expired entries while we have the lock
            blacklist.retain(|_, v| v.expires > now);
        }

        // Acquire pending and order locks in consistent order
        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        let is_new = if let Some(existing) = pending.get_mut(&market_id) {
            // Update existing entry with fresh prices
            existing.update(request);
            false
        } else {
            // New entry
            pending.insert(market_id, PendingArb::new(request, pair));
            order.push_back(market_id);
            true
        };

        // Only notify TUI on new entries to avoid flooding with updates.
        // The TUI re-renders every 100ms anyway, so price updates will still show.
        if is_new {
            let _ = self.update_tx.try_send(());
        }

        is_new
    }

    /// Get the current front pending arb (if any)
    pub async fn front(&self) -> Option<PendingArb> {
        let pending = self.pending.read().await;
        let order = self.order.read().await;

        order.front().and_then(|id| pending.get(id).cloned())
    }

    /// Get count of pending arbs
    pub async fn len(&self) -> usize {
        self.pending.read().await.len()
    }

    /// Check if queue is empty
    pub async fn is_empty(&self) -> bool {
        self.pending.read().await.is_empty()
    }

    /// Remove and return the front arb (after user decision)
    pub async fn pop_front(&self) -> Option<PendingArb> {
        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        if let Some(market_id) = order.pop_front() {
            pending.remove(&market_id)
        } else {
            None
        }
    }

    /// Add a market to the blacklist for 5 minutes
    pub async fn blacklist_market(&self, market_id: u16) {
        let mut blacklist = self.blacklist.write().await;
        blacklist.insert(market_id, BlacklistEntry {
            expires: Instant::now() + Duration::from_secs(300), // 5 minutes
        });
    }

    /// Check if arb is still valid with detailed cost info
    pub fn validate_arb_detailed(&self, arb: &PendingArb) -> Option<ValidationResult> {
        // Test arbs use synthetic prices, skip validation
        if arb.request.is_test {
            return Some(ValidationResult {
                is_valid: true,
                original_cost: arb.request.yes_price + arb.request.no_price,
                current_cost: arb.request.yes_price + arb.request.no_price,
            });
        }

        let market = self.state.get_by_id(arb.request.market_id)?;

        // Get current prices from orderbook
        let (k_yes, k_no, _, _) = market.kalshi.load();
        let (p_yes, p_no, _, _) = market.poly.load();

        // Calculate original cost (from when arb was queued)
        let original_cost = arb.request.yes_price + arb.request.no_price + match arb.request.arb_type {
            ArbType::PolyYesKalshiNo => kalshi_fee(arb.request.no_price),
            ArbType::KalshiYesPolyNo => kalshi_fee(arb.request.yes_price),
            ArbType::PolyOnly => 0,
            ArbType::KalshiOnly => kalshi_fee(arb.request.yes_price) + kalshi_fee(arb.request.no_price),
        };

        // Calculate current cost based on arb type
        let current_cost = match arb.request.arb_type {
            ArbType::PolyYesKalshiNo => {
                let fee = kalshi_fee(k_no);
                p_yes + k_no + fee
            }
            ArbType::KalshiYesPolyNo => {
                let fee = kalshi_fee(k_yes);
                k_yes + fee + p_no
            }
            ArbType::PolyOnly => p_yes + p_no,
            ArbType::KalshiOnly => {
                let fee_yes = kalshi_fee(k_yes);
                let fee_no = kalshi_fee(k_no);
                k_yes + fee_yes + k_no + fee_no
            }
        };

        Some(ValidationResult {
            is_valid: current_cost < 100,
            original_cost,
            current_cost,
        })
    }

    /// Re-validate arb using ArbOpportunity with current orderbook prices.
    /// Returns Some(ArbOpportunity) if a valid arb still exists, None otherwise.
    ///
    /// This can be used to get a fresh ArbOpportunity with current prices for execution,
    /// rather than using the potentially stale prices from when the arb was queued.
    #[allow(dead_code)]
    pub fn validate_arb(&self, arb: &PendingArb, config: &ArbConfig) -> Option<ArbOpportunity> {
        // Test arbs always pass validation
        if arb.request.is_test {
            return Some(ArbOpportunity::new(
                arb.request.market_id,
                (arb.request.yes_price, arb.request.no_price, arb.request.yes_size, arb.request.no_size),
                (arb.request.yes_price, arb.request.no_price, arb.request.yes_size, arb.request.no_size),
                config,
                0,
            ));
        }

        let market = self.state.get_by_id(arb.request.market_id)?;

        // Get current prices from orderbook
        let kalshi = market.kalshi.load();
        let poly = market.poly.load();

        // Use ArbOpportunity to re-validate with current prices
        let opp = ArbOpportunity::new(arb.request.market_id, kalshi, poly, config, 0);

        if opp.is_valid() {
            Some(opp)
        } else {
            None
        }
    }
}
