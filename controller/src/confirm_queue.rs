//! Confirmation queue for pending arbitrage opportunities.
//!
//! Manages a keyed queue of arbs awaiting user confirmation. One slot per market,
//! new arbs for existing markets replace old data to keep prices fresh.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use crate::config::{build_polymarket_url, KALSHI_WEB_BASE};
use crate::types::{ArbType, FastExecutionRequest, GlobalState, MarketPair, PriceCents, kalshi_fee_cents};

/// A pending arbitrage opportunity awaiting confirmation
#[derive(Debug, Clone)]
pub struct PendingArb {
    /// Original execution request
    pub request: FastExecutionRequest,
    /// Market pair info (for display)
    pub pair: Arc<MarketPair>,
    /// Number of times this arb has been detected while pending
    pub detection_count: u32,
    /// First detection timestamp
    pub first_detected: Instant,
    /// Most recent detection timestamp
    pub last_detected: Instant,
    /// Kalshi market URL
    pub kalshi_url: String,
    /// Polymarket event URL
    pub poly_url: String,
}

impl PendingArb {
    pub fn new(request: FastExecutionRequest, pair: Arc<MarketPair>) -> Self {
        let kalshi_url = format!("{}/{}", KALSHI_WEB_BASE, pair.kalshi_market_ticker);
        let poly_url = build_polymarket_url(&pair.league, &pair.poly_slug);
        let now = Instant::now();

        Self {
            request,
            pair,
            detection_count: 1,
            first_detected: now,
            last_detected: now,
            kalshi_url,
            poly_url,
        }
    }

    /// Update with fresher price data
    pub fn update(&mut self, request: FastExecutionRequest) {
        self.request = request;
        self.detection_count += 1;
        self.last_detected = Instant::now();
    }

    /// Calculate profit in cents based on current request prices
    pub fn profit_cents(&self) -> i16 {
        self.request.profit_cents()
    }

    /// Calculate max contracts from size
    pub fn max_contracts(&self) -> i64 {
        (self.request.yes_size.min(self.request.no_size) / 100) as i64
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

    /// Push a new arb opportunity (or update existing for same market)
    pub async fn push(&self, request: FastExecutionRequest, pair: Arc<MarketPair>) {
        let market_id = request.market_id;

        // Check blacklist and clean expired entries in one lock acquisition
        {
            let mut blacklist = self.blacklist.write().await;
            let now = Instant::now();

            // Check if this market is blacklisted
            if let Some(entry) = blacklist.get(&market_id) {
                if entry.expires > now {
                    return; // Still blacklisted
                }
            }

            // Clean expired entries while we have the lock
            blacklist.retain(|_, v| v.expires > now);
        }

        // Acquire pending and order locks in consistent order
        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        if let Some(existing) = pending.get_mut(&market_id) {
            // Update existing entry with fresh prices
            existing.update(request);
        } else {
            // New entry
            pending.insert(market_id, PendingArb::new(request, pair));
            order.push_back(market_id);
        }

        // Notify TUI
        let _ = self.update_tx.try_send(());
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

    /// Remove a market from pending (e.g., when prices invalidate it)
    pub async fn remove(&self, market_id: u16) -> Option<PendingArb> {
        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        order.retain(|id| *id != market_id);
        pending.remove(&market_id)
    }

    /// Check if arb is still valid (prices haven't moved)
    pub fn validate_arb(&self, arb: &PendingArb) -> bool {
        let market = match self.state.get_by_id(arb.request.market_id) {
            Some(m) => m,
            None => return false,
        };

        // Get current prices from orderbook
        let (k_yes, k_no, _, _) = market.kalshi.load();
        let (p_yes, p_no, _, _) = market.poly.load();

        // Calculate current cost based on arb type
        let current_cost = match arb.request.arb_type {
            ArbType::PolyYesKalshiNo => {
                let fee = kalshi_fee_cents(k_no);
                p_yes + k_no + fee
            }
            ArbType::KalshiYesPolyNo => {
                let fee = kalshi_fee_cents(k_yes);
                k_yes + fee + p_no
            }
            ArbType::PolyOnly => p_yes + p_no,
            ArbType::KalshiOnly => {
                let fee_yes = kalshi_fee_cents(k_yes);
                let fee_no = kalshi_fee_cents(k_no);
                k_yes + fee_yes + k_no + fee_no
            }
        };

        // Still profitable if total cost < 100 cents
        current_cost < 100
    }

    /// Get current prices for an arb (for live display updates)
    pub fn get_current_prices(&self, market_id: u16) -> Option<(PriceCents, PriceCents, PriceCents, PriceCents)> {
        let market = self.state.get_by_id(market_id)?;
        let (k_yes, k_no, _, _) = market.kalshi.load();
        let (p_yes, p_no, _, _) = market.poly.load();
        Some((k_yes, k_no, p_yes, p_no))
    }
}
