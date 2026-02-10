//! Confirmation queue for pending arbitrage opportunities.
//!
//! Manages a keyed queue of arbs awaiting user confirmation. One slot per market,
//! new arbs for existing markets replace old data to keep prices fresh.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use crate::arb::kalshi_fee;
use crate::config::{build_polymarket_url, KALSHI_WEB_BASE};
use crate::types::{ArbType, ArbOpportunity, GlobalState, MarketPair, PriceCents};

/// A pending arbitrage opportunity awaiting confirmation
#[derive(Debug, Clone)]
pub struct PendingArb {
    /// Original execution request
    pub request: ArbOpportunity,
    /// Market pair info (for display)
    pub pair: Arc<MarketPair>,
    /// Number of times this arb has been detected while pending
    pub detection_count: u32,
    /// First detection timestamp
    #[allow(dead_code)]
    pub first_detected: Instant,
    /// Most recent detection timestamp
    pub last_detected: Instant,
    /// Kalshi market URL
    pub kalshi_url: String,
    /// Polymarket event URL
    pub poly_url: String,
}

impl PendingArb {
    pub fn new(request: ArbOpportunity, pair: Arc<MarketPair>) -> Self {
        // Build Kalshi URL: https://kalshi.com/markets/{series}/{slug}/{event_ticker}
        let kalshi_series = pair.kalshi_event_ticker
            .split('-')
            .next()
            .unwrap_or(&pair.kalshi_event_ticker)
            .to_lowercase();
        let kalshi_event_ticker_lower = pair.kalshi_event_ticker.to_lowercase();
        let kalshi_url = format!("{}/{}/{}/{}", KALSHI_WEB_BASE, kalshi_series, pair.kalshi_event_slug, kalshi_event_ticker_lower);
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
    pub fn update(&mut self, request: ArbOpportunity) {
        self.request = request;
        self.detection_count += 1;
        self.last_detected = Instant::now();
    }

    /// Calculate profit in cents based on current request prices
    pub fn profit_cents(&self) -> i16 {
        self.request.profit_cents()
    }

    /// Calculate max contracts based on available size and prices.
    /// Delegates to ArbOpportunity::max_contracts().
    pub fn max_contracts(&self) -> u16 {
        self.request.max_contracts()
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
    /// Note: Size validation is done upstream in ArbOpportunity::detect()
    pub async fn push(&self, request: ArbOpportunity, pair: Arc<MarketPair>) -> bool {
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
            if self.update_tx.try_send(()).is_err() {
                // Channel full or closed - TUI may be slow or shut down.
                // Log at debug level since TUI re-renders periodically anyway.
                tracing::debug!("[CONFIRM] TUI notification channel full or closed");
            }
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

    /// Remove a market from pending (e.g., when prices invalidate it)
    #[allow(dead_code)]
    pub async fn remove(&self, market_id: u16) -> Option<PendingArb> {
        let mut pending = self.pending.write().await;
        let mut order = self.order.write().await;

        order.retain(|id| *id != market_id);
        pending.remove(&market_id)
    }

    /// Check if arb is still valid (prices haven't moved)
    #[allow(dead_code)]
    pub fn validate_arb(&self, arb: &PendingArb) -> bool {
        self.validate_arb_detailed(arb).map(|r| r.is_valid).unwrap_or(false)
    }

    /// Check if arb is still valid with detailed cost info.
    /// Returns None if the market is no longer in global state (race condition or state inconsistency).
    pub fn validate_arb_detailed(&self, arb: &PendingArb) -> Option<ValidationResult> {
        // Test arbs use synthetic prices, skip validation
        if arb.request.is_test {
            return Some(ValidationResult {
                is_valid: true,
                original_cost: arb.request.yes_price + arb.request.no_price,
                current_cost: arb.request.yes_price + arb.request.no_price,
            });
        }

        let market = match self.state.get_by_id(arb.request.market_id) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    "[CONFIRM] Market {} not found during validation for {}",
                    arb.request.market_id,
                    arb.pair.description
                );
                return None;
            }
        };

        // Get current prices from orderbook
        let (k_yes, k_no, _, _) = market.kalshi.read().top_of_book();
        let (p_yes, p_no, _, _) = market.poly.read().top_of_book();

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

    /// Get current prices for an arb (for live display updates)
    #[allow(dead_code)]
    pub fn get_current_prices(&self, market_id: u16) -> Option<(PriceCents, PriceCents, PriceCents, PriceCents)> {
        let market = self.state.get_by_id(market_id)?;
        let (k_yes, k_no, _, _) = market.kalshi.read().top_of_book();
        let (p_yes, p_no, _, _) = market.poly.read().top_of_book();
        Some((k_yes, k_no, p_yes, p_no))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MarketType;

    /// Create a test MarketPair with minimal fields
    fn test_market_pair() -> Arc<MarketPair> {
        Arc::new(MarketPair {
            pair_id: "test-pair".into(),
            league: "nba".into(),
            market_type: MarketType::Moneyline,
            description: "Test Market".into(),
            kalshi_event_ticker: "KXNBA-TEST".into(),
            kalshi_market_ticker: "KXNBA-TEST-MKT".into(),
            kalshi_event_slug: "test-event".into(),
            poly_slug: "nba-test-2026-01-01".into(),
            poly_yes_token: "0x1234".into(),
            poly_no_token: "0x5678".into(),
            line_value: None,
            team_suffix: None,
            neg_risk: false,
        })
    }

    /// Create a test ArbOpportunity
    fn test_request(yes_price: u16, no_price: u16, yes_size: u16, no_size: u16) -> ArbOpportunity {
        ArbOpportunity {
            market_id: 1,
            yes_price,
            no_price,
            yes_size,
            no_size,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        }
    }

    // =========================================================================
    // PendingArb tests
    // =========================================================================

    #[test]
    fn test_pending_arb_max_contracts_basic() {
        // YES: 400/40 = 10 contracts, NO: 600/50 = 12 contracts
        // max_contracts = min(10, 12) = 10
        let request = test_request(40, 50, 400, 600);
        let pending = PendingArb::new(request, test_market_pair());

        assert_eq!(pending.max_contracts(), 10);
    }

    #[test]
    fn test_pending_arb_max_contracts_no_side_limiting() {
        // YES: 800/40 = 20 contracts, NO: 250/50 = 5 contracts
        // max_contracts = min(20, 5) = 5
        let request = test_request(40, 50, 800, 250);
        let pending = PendingArb::new(request, test_market_pair());

        assert_eq!(pending.max_contracts(), 5);
    }

    #[test]
    fn test_pending_arb_max_contracts_zero_yes_price() {
        // Division by zero protection
        let request = test_request(0, 50, 400, 600);
        let pending = PendingArb::new(request, test_market_pair());

        assert_eq!(pending.max_contracts(), 0);
    }

    #[test]
    fn test_pending_arb_max_contracts_zero_no_price() {
        // Division by zero protection
        let request = test_request(40, 0, 400, 600);
        let pending = PendingArb::new(request, test_market_pair());

        assert_eq!(pending.max_contracts(), 0);
    }

    #[test]
    fn test_pending_arb_max_contracts_both_prices_zero() {
        let request = test_request(0, 0, 400, 600);
        let pending = PendingArb::new(request, test_market_pair());

        assert_eq!(pending.max_contracts(), 0);
    }

    #[test]
    fn test_pending_arb_update_increments_detection_count() {
        let request1 = test_request(40, 50, 400, 600);
        let request2 = test_request(41, 51, 500, 700);
        let mut pending = PendingArb::new(request1, test_market_pair());

        assert_eq!(pending.detection_count, 1);

        pending.update(request2);

        assert_eq!(pending.detection_count, 2);
        assert_eq!(pending.request.yes_price, 41);
        assert_eq!(pending.request.no_price, 51);
    }

    #[test]
    fn test_pending_arb_profit_cents_delegates_to_request() {
        // This tests that profit_cents() correctly delegates to request.profit_cents()
        let request = test_request(40, 50, 400, 600);
        let pending = PendingArb::new(request.clone(), test_market_pair());

        // profit_cents should match the request's calculation
        assert_eq!(pending.profit_cents(), request.profit_cents());
    }

    #[test]
    fn test_pending_arb_urls_constructed_correctly() {
        let request = test_request(40, 50, 400, 600);
        let pending = PendingArb::new(request, test_market_pair());

        // Kalshi URL should contain the event ticker (lowercase) and event slug
        // Format: {base}/{series}/{slug}/{event_ticker}
        assert!(pending.kalshi_url.contains("kxnba-test"), "Expected kxnba-test in {}", pending.kalshi_url);
        assert!(pending.kalshi_url.contains("test-event"), "Expected test-event in {}", pending.kalshi_url);

        // Polymarket URL should be built from league and slug
        assert!(pending.poly_url.contains("nba"));
    }
}
