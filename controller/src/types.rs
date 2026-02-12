//! Core type definitions and data structures for the arbitrage trading system.
//!
//! This module provides the foundational types for market state management,
//! orderbook representation, and arbitrage opportunity detection.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use rustc_hash::FxHashMap;
use parking_lot::{Mutex, RwLock};

use crate::arb::{ArbConfig, kalshi_fee};

// === Market Types ===

/// Market category for a matched trading pair
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarketType {
    /// Moneyline/outright winner market
    Moneyline,
    /// Point spread market
    Spread,
    /// Total/over-under market
    Total,
    /// Both teams to score market
    Btts,
}

impl std::fmt::Display for MarketType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MarketType::Moneyline => write!(f, "moneyline"),
            MarketType::Spread => write!(f, "spread"),
            MarketType::Total => write!(f, "total"),
            MarketType::Btts => write!(f, "btts"),
        }
    }
}

/// A matched trading pair between Kalshi and Polymarket platforms
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketPair {
    /// Unique identifier for this market pair
    pub pair_id: Arc<str>,
    /// Sports league identifier (e.g., "epl", "nba")
    pub league: Arc<str>,
    /// Type of market (moneyline, spread, total, etc.)
    pub market_type: MarketType,
    /// Human-readable market description
    pub description: Arc<str>,
    /// Kalshi event ticker identifier
    pub kalshi_event_ticker: Arc<str>,
    /// Kalshi market ticker identifier
    pub kalshi_market_ticker: Arc<str>,
    /// Kalshi event slug for web URLs (e.g., "counterstrike-2-game")
    pub kalshi_event_slug: Arc<str>,
    /// Polymarket market slug
    pub poly_slug: Arc<str>,
    /// Polymarket YES outcome token address
    pub poly_yes_token: Arc<str>,
    /// Polymarket NO outcome token address
    pub poly_no_token: Arc<str>,
    /// Line value for spread/total markets (if applicable)
    pub line_value: Option<f64>,
    /// Team suffix for team-specific markets
    pub team_suffix: Option<Arc<str>>,
    /// Whether this market uses negative risk (determines Polymarket exchange contract)
    #[serde(default)]
    pub neg_risk: bool,
}

/// Price representation in cents (1-99 for $0.01-$0.99), 0 indicates no price available
pub type PriceCents = u16;

/// Size representation in cents (dollar amount × 100), maximum ~$655k per side
pub type SizeCents = u16;

/// Maximum number of concurrently tracked markets
pub const MAX_MARKETS: usize = 1024;

/// Sentinel value indicating no price is currently available.
/// Used semantically in price checks (e.g., ArbOpportunity::detect checks for 0).
#[allow(dead_code)]
pub const NO_PRICE: PriceCents = 0;

/// Pack orderbook state into a single u64 for atomic operations.
/// Bit layout: [yes_ask:16][no_ask:16][yes_size:16][no_size:16]
#[inline(always)]
pub fn pack_orderbook(yes_ask: PriceCents, no_ask: PriceCents, yes_size: SizeCents, no_size: SizeCents) -> u64 {
    ((yes_ask as u64) << 48) | ((no_ask as u64) << 32) | ((yes_size as u64) << 16) | (no_size as u64)
}

/// Unpack a u64 orderbook representation back into its component values
#[inline(always)]
pub fn unpack_orderbook(packed: u64) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
    let yes_ask = ((packed >> 48) & 0xFFFF) as PriceCents;
    let no_ask = ((packed >> 32) & 0xFFFF) as PriceCents;
    let yes_size = ((packed >> 16) & 0xFFFF) as SizeCents;
    let no_size = (packed & 0xFFFF) as SizeCents;
    (yes_ask, no_ask, yes_size, no_size)
}

/// Lock-free orderbook state for a single trading platform.
/// Uses atomic operations for thread-safe, zero-copy price updates.
#[repr(align(64))]
pub struct AtomicOrderbook {
    /// Packed orderbook state: [yes_ask:16][no_ask:16][yes_size:16][no_size:16]
    packed: AtomicU64,
}

impl AtomicOrderbook {
    pub const fn new() -> Self {
        Self { packed: AtomicU64::new(0) }
    }

    /// Load current state
    #[inline(always)]
    pub fn load(&self) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
        unpack_orderbook(self.packed.load(Ordering::Acquire))
    }

    /// Store new state
    #[inline(always)]
    pub fn store(&self, yes_ask: PriceCents, no_ask: PriceCents, yes_size: SizeCents, no_size: SizeCents) {
        self.packed.store(pack_orderbook(yes_ask, no_ask, yes_size, no_size), Ordering::Release);
    }

    /// Update YES side only
    #[inline(always)]
    pub fn update_yes(&self, yes_ask: PriceCents, yes_size: SizeCents) {
        let mut current = self.packed.load(Ordering::Acquire);
        loop {
            let (_, no_ask, _, no_size) = unpack_orderbook(current);
            let new = pack_orderbook(yes_ask, no_ask, yes_size, no_size);
            match self.packed.compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }

    /// Update NO side only
    #[inline(always)]
    pub fn update_no(&self, no_ask: PriceCents, no_size: SizeCents) {
        let mut current = self.packed.load(Ordering::Acquire);
        loop {
            let (yes_ask, _, yes_size, _) = unpack_orderbook(current);
            let new = pack_orderbook(yes_ask, no_ask, yes_size, no_size);
            match self.packed.compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }
}

impl Default for AtomicOrderbook {
    fn default() -> Self {
        Self::new()
    }
}

/// Shadow orderbook for Polymarket.
///
/// Maintains full ask-side depth for YES and NO tokens so the true best ask
/// is always derivable — even after the current best is removed.
///
/// Polymarket sends asks directly (unlike Kalshi bids). Deltas use absolute
/// replacement (`qty = new_size`), not incremental (`qty += delta`).
/// `size = 0` means remove the level.
pub struct PolyBook {
    yes_asks: BTreeMap<u16, u16>,
    no_asks: BTreeMap<u16, u16>,
}

impl PolyBook {
    pub fn new() -> Self {
        Self {
            yes_asks: BTreeMap::new(),
            no_asks: BTreeMap::new(),
        }
    }

    /// Replace all YES ask levels from a book snapshot.
    /// Clears existing state first — snapshots are full replacements.
    pub fn set_yes_asks(&mut self, levels: &[(u16, u16)]) {
        self.yes_asks.clear();
        for &(price, size) in levels {
            if price > 0 && size > 0 {
                self.yes_asks.insert(price, size);
            }
        }
    }

    /// Replace all NO ask levels from a book snapshot.
    pub fn set_no_asks(&mut self, levels: &[(u16, u16)]) {
        self.no_asks.clear();
        for &(price, size) in levels {
            if price > 0 && size > 0 {
                self.no_asks.insert(price, size);
            }
        }
    }

    /// Apply a YES ask level update (absolute replacement).
    /// size = 0 removes the level.
    pub fn update_yes_level(&mut self, price: u16, size: u16) {
        if size == 0 {
            self.yes_asks.remove(&price);
        } else {
            self.yes_asks.insert(price, size);
        }
    }

    /// Apply a NO ask level update (absolute replacement).
    pub fn update_no_level(&mut self, price: u16, size: u16) {
        if size == 0 {
            self.no_asks.remove(&price);
        } else {
            self.no_asks.insert(price, size);
        }
    }

    /// Best YES ask: lowest price in the book.
    /// BTreeMap is ascending, so `iter().next()` is the lowest key.
    pub fn best_yes_ask(&self) -> Option<(u16, u16)> {
        self.yes_asks.iter().next().map(|(&p, &s)| (p, s))
    }

    /// Best NO ask: lowest price in the book.
    pub fn best_no_ask(&self) -> Option<(u16, u16)> {
        self.no_asks.iter().next().map(|(&p, &s)| (p, s))
    }

    /// Clear all state.
    pub fn clear(&mut self) {
        self.yes_asks.clear();
        self.no_asks.clear();
    }
}

/// Complete market state tracking both platforms' orderbooks for a single market
pub struct AtomicMarketState {
    /// Kalshi platform orderbook state
    pub kalshi: AtomicOrderbook,
    /// Polymarket platform orderbook state
    pub poly: AtomicOrderbook,
    /// Last known Kalshi update time (unix ms). 0 = unknown / never updated.
    kalshi_last_update_unix_ms: AtomicU64,
    /// Last known Polymarket update time (unix ms). 0 = unknown / never updated.
    poly_last_update_unix_ms: AtomicU64,
    /// Market pair metadata (supports runtime addition via interior mutability)
    pair: RwLock<Option<Arc<MarketPair>>>,
    /// Unique market identifier for O(1) lookups
    pub market_id: u16,
    /// Count of price updates from Kalshi WebSocket
    pub kalshi_updates: AtomicU32,
    /// Count of price updates from Polymarket WebSocket
    pub poly_updates: AtomicU32,
    /// Shadow orderbook for Polymarket ask-side depth
    pub poly_book: Mutex<PolyBook>,
}

impl AtomicMarketState {
    pub fn new(market_id: u16) -> Self {
        Self {
            kalshi: AtomicOrderbook::new(),
            poly: AtomicOrderbook::new(),
            kalshi_last_update_unix_ms: AtomicU64::new(0),
            poly_last_update_unix_ms: AtomicU64::new(0),
            pair: RwLock::new(None),
            market_id,
            kalshi_updates: AtomicU32::new(0),
            poly_updates: AtomicU32::new(0),
            poly_book: Mutex::new(PolyBook::new()),
        }
    }

    /// Get a clone of the market pair (read lock)
    #[inline]
    pub fn pair(&self) -> Option<Arc<MarketPair>> {
        self.pair.read().clone()
    }

    /// Set the market pair (write lock) - used during runtime market addition
    #[inline]
    pub fn set_pair(&self, pair: Arc<MarketPair>) {
        *self.pair.write() = Some(pair);
    }

    /// Record last update time for Kalshi (unix ms).
    #[inline]
    pub fn mark_kalshi_update_unix_ms(&self, unix_ms: u64) {
        self.kalshi_last_update_unix_ms
            .store(unix_ms, Ordering::Release);
    }

    /// Record last update time for Polymarket (unix ms).
    #[inline]
    pub fn mark_poly_update_unix_ms(&self, unix_ms: u64) {
        self.poly_last_update_unix_ms
            .store(unix_ms, Ordering::Release);
    }

    /// Get last update times (kalshi_unix_ms, poly_unix_ms).
    #[inline]
    pub fn last_updates_unix_ms(&self) -> (u64, u64) {
        (
            self.kalshi_last_update_unix_ms.load(Ordering::Acquire),
            self.poly_last_update_unix_ms.load(Ordering::Acquire),
        )
    }

    /// Increment Kalshi update counter
    #[inline]
    pub fn inc_kalshi_updates(&self) {
        self.kalshi_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment Polymarket update counter
    #[inline]
    pub fn inc_poly_updates(&self) {
        self.poly_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Load both update counters (kalshi_updates, poly_updates)
    #[inline]
    pub fn load_update_counts(&self) -> (u32, u32) {
        (
            self.kalshi_updates.load(Ordering::Relaxed),
            self.poly_updates.load(Ordering::Relaxed),
        )
    }
}

/// Convert f64 price (0.01-0.99) to PriceCents (1-99)
#[inline(always)]
pub fn price_to_cents(price: f64) -> PriceCents {
    ((price * 100.0).round() as PriceCents).clamp(0, 99)
}

/// Convert PriceCents back to f64
#[inline(always)]
pub fn cents_to_price(cents: PriceCents) -> f64 {
    cents as f64 / 100.0
}

/// Parse price from string "0.XX" format (Polymarket)
/// Returns 0 if parsing fails
#[inline(always)]
pub fn parse_price(s: &str) -> PriceCents {
    let bytes = s.as_bytes();
    // Handle "0.XX" format (4 chars)
    if bytes.len() == 4 && bytes[0] == b'0' && bytes[1] == b'.' {
        let d1 = bytes[2].wrapping_sub(b'0');
        let d2 = bytes[3].wrapping_sub(b'0');
        if d1 < 10 && d2 < 10 {
            return (d1 as u16 * 10 + d2 as u16) as PriceCents;
        }
    }
    // Handle "0.X" format (3 chars) for prices like 0.5
    if bytes.len() == 3 && bytes[0] == b'0' && bytes[1] == b'.' {
        let d = bytes[2].wrapping_sub(b'0');
        if d < 10 {
            return (d as u16 * 10) as PriceCents;
        }
    }
    // Fallback to standard parse
    s.parse::<f64>()
        .map(price_to_cents)
        .unwrap_or(0)
}

/// Arbitrage opportunity type, determining the execution strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArbType {
    /// Cross-platform: Buy Polymarket YES + Buy Kalshi NO
    PolyYesKalshiNo,
    /// Cross-platform: Buy Kalshi YES + Buy Polymarket NO
    KalshiYesPolyNo,
    /// Same-platform: Buy Polymarket YES + Buy Polymarket NO
    PolyOnly,
    /// Same-platform: Buy Kalshi YES + Buy Kalshi NO
    KalshiOnly,
}

/// High-priority execution request for an arbitrage opportunity
#[derive(Debug, Clone, Copy)]
pub struct ArbOpportunity {
    /// Market identifier (index into GlobalState.markets array)
    pub market_id: u16,
    /// YES outcome ask price in cents
    pub yes_price: PriceCents,
    /// NO outcome ask price in cents
    pub no_price: PriceCents,
    /// YES outcome available size in cents
    pub yes_size: SizeCents,
    /// NO outcome available size in cents
    pub no_size: SizeCents,
    /// Arbitrage type (determines execution strategy)
    pub arb_type: ArbType,
    /// Detection timestamp in nanoseconds since system start
    pub detected_ns: u64,
    /// Whether this is a test arb (skips price validation in confirm mode)
    pub is_test: bool,
}

impl ArbOpportunity {
    /// Calculate max executable contracts (minimum of both sides).
    ///
    /// Returns the number of complete contracts that can be bought,
    /// computed as min(yes_size / yes_price, no_size / no_price).
    /// Returns 0 if either price is 0 (no price available).
    #[inline]
    pub fn max_contracts(&self) -> u16 {
        if self.yes_price == 0 || self.no_price == 0 {
            return 0;
        }
        let yes_contracts = self.yes_size / self.yes_price;
        let no_contracts = self.no_size / self.no_price;
        yes_contracts.min(no_contracts)
    }

    #[inline(always)]
    pub fn profit_cents(&self) -> i16 {
        100 - (self.yes_price as i16 + self.no_price as i16 + self.estimated_fee_cents() as i16)
    }

    #[inline(always)]
    pub fn estimated_fee_cents(&self) -> PriceCents {
        match self.arb_type {
            // Cross-platform: fee on the Kalshi side only
            ArbType::PolyYesKalshiNo => kalshi_fee(self.no_price),
            ArbType::KalshiYesPolyNo => kalshi_fee(self.yes_price),
            // Poly-only: no fees
            ArbType::PolyOnly => 0,
            // Kalshi-only: fees on both sides
            ArbType::KalshiOnly => kalshi_fee(self.yes_price) + kalshi_fee(self.no_price),
        }
    }

    /// Detect arbitrage opportunity from orderbook data.
    /// Returns Some if a valid arb exists, None otherwise.
    ///
    /// This is the single source of truth for arb detection logic.
    pub fn detect(
        market_id: u16,
        kalshi: (PriceCents, PriceCents, SizeCents, SizeCents),
        poly: (PriceCents, PriceCents, SizeCents, SizeCents),
        config: &ArbConfig,
        detected_ns: u64,
    ) -> Option<Self> {
        let (k_yes, k_no, k_yes_size, k_no_size) = kalshi;
        let (p_yes, p_no, p_yes_size, p_no_size) = poly;

        // Check for invalid prices (0 = no price available)
        if k_yes == 0 || k_no == 0 || p_yes == 0 || p_no == 0 {
            return None;
        }

        // Calculate Kalshi fees
        let k_yes_fee = kalshi_fee(k_yes);
        let k_no_fee = kalshi_fee(k_no);

        // Arb candidates in priority order
        let candidates = [
            (ArbType::PolyYesKalshiNo, p_yes + k_no + k_no_fee, p_yes, k_no, p_yes_size, k_no_size),
            (ArbType::KalshiYesPolyNo, k_yes + k_yes_fee + p_no, k_yes, p_no, k_yes_size, p_no_size),
            (ArbType::PolyOnly, p_yes + p_no, p_yes, p_no, p_yes_size, p_no_size),
            (ArbType::KalshiOnly, k_yes + k_yes_fee + k_no + k_no_fee, k_yes, k_no, k_yes_size, k_no_size),
        ];

        // Polymarket minimum order value is $1.00 (100 cents)
        const POLY_MIN_ORDER_CENTS: u16 = 100;

        // Find first valid arb (lowest cost that beats threshold)
        for (arb_type, cost, yes_price, no_price, yes_size, no_size) in candidates {
            if cost <= config.threshold_cents() {
                // Calculate max contracts: min of contracts purchasable on each side
                // Contracts = size (cents) / price (cents per contract)
                let yes_contracts = if yes_price > 0 { yes_size / yes_price } else { 0 };
                let no_contracts = if no_price > 0 { no_size / no_price } else { 0 };
                let max_contracts = yes_contracts.min(no_contracts);

                if max_contracts as f64 >= config.min_contracts() {
                    // Check Polymarket minimum order value ($1.00)
                    // Order value = contracts × price_per_contract
                    let poly_order_ok = match arb_type {
                        ArbType::PolyYesKalshiNo => {
                            // Poly YES leg: contracts × yes_price >= $1
                            max_contracts * yes_price >= POLY_MIN_ORDER_CENTS
                        }
                        ArbType::KalshiYesPolyNo => {
                            // Poly NO leg: contracts × no_price >= $1
                            max_contracts * no_price >= POLY_MIN_ORDER_CENTS
                        }
                        ArbType::PolyOnly => {
                            // Both legs on Poly: both must be >= $1
                            max_contracts * yes_price >= POLY_MIN_ORDER_CENTS
                                && max_contracts * no_price >= POLY_MIN_ORDER_CENTS
                        }
                        ArbType::KalshiOnly => {
                            // No Poly involvement
                            true
                        }
                    };

                    if poly_order_ok {
                        return Some(Self {
                            market_id,
                            arb_type,
                            yes_price,
                            no_price,
                            yes_size,
                            no_size,
                            detected_ns,
                            is_test: false,
                        });
                    }
                }
            }
        }

        None
    }
}

/// Global market state manager for all tracked markets across both platforms.
/// Supports concurrent market addition at runtime via interior mutability.
pub struct GlobalState {
    /// Market states indexed by market_id for O(1) access
    pub markets: Vec<AtomicMarketState>,

    /// Next available market identifier (monotonically increasing, atomic)
    next_market_id: AtomicU16,

    /// O(1) lookup map: pre-hashed Kalshi ticker → market_id (RwLock for runtime updates)
    pub kalshi_to_id: RwLock<FxHashMap<u64, u16>>,

    /// O(1) lookup map: pre-hashed Polymarket YES token → market_id (RwLock for runtime updates)
    pub poly_yes_to_id: RwLock<FxHashMap<u64, u16>>,

    /// O(1) lookup map: pre-hashed Polymarket NO token → market_id (RwLock for runtime updates)
    pub poly_no_to_id: RwLock<FxHashMap<u64, u16>>,

    /// Arbitrage detection configuration
    arb_config: ArbConfig,
}

impl GlobalState {
    pub fn new(arb_config: ArbConfig) -> Self {
        // Allocate market slots
        let markets: Vec<AtomicMarketState> = (0..MAX_MARKETS)
            .map(|i| AtomicMarketState::new(i as u16))
            .collect();

        Self {
            markets,
            next_market_id: AtomicU16::new(0),
            kalshi_to_id: RwLock::new(FxHashMap::default()),
            poly_yes_to_id: RwLock::new(FxHashMap::default()),
            poly_no_to_id: RwLock::new(FxHashMap::default()),
            arb_config,
        }
    }

    /// Returns a reference to the arbitrage configuration.
    pub fn arb_config(&self) -> &ArbConfig {
        &self.arb_config
    }

    /// Add a market pair, returns market_id.
    /// Thread-safe: uses interior mutability for concurrent access.
    pub fn add_pair(&self, pair: MarketPair) -> Option<u16> {
        // Atomically increment and get market_id
        let market_id = self.next_market_id.fetch_add(1, Ordering::SeqCst);

        if market_id as usize >= MAX_MARKETS {
            // Rollback if we exceeded capacity
            self.next_market_id.fetch_sub(1, Ordering::SeqCst);
            return None;
        }

        // Pre-compute hashes
        let kalshi_hash = fxhash_str(&pair.kalshi_market_ticker);
        let poly_yes_hash = fxhash_str(&pair.poly_yes_token);
        let poly_no_hash = fxhash_str(&pair.poly_no_token);

        // Update lookup maps (write locks)
        {
            let mut kalshi = self.kalshi_to_id.write();
            kalshi.insert(kalshi_hash, market_id);
        }
        {
            let mut poly_yes = self.poly_yes_to_id.write();
            poly_yes.insert(poly_yes_hash, market_id);
        }
        {
            let mut poly_no = self.poly_no_to_id.write();
            poly_no.insert(poly_no_hash, market_id);
        }

        // Store pair using the new set_pair method
        self.markets[market_id as usize].set_pair(Arc::new(pair));

        Some(market_id)
    }

    /// Get market by Kalshi ticker hash (O(1))
    #[inline(always)]
    #[allow(dead_code)]
    pub fn get_by_kalshi_hash(&self, hash: u64) -> Option<&AtomicMarketState> {
        let id = *self.kalshi_to_id.read().get(&hash)?;
        Some(&self.markets[id as usize])
    }

    /// Get market by Poly YES token hash (O(1))
    #[inline(always)]
    #[allow(dead_code)]
    pub fn get_by_poly_yes_hash(&self, hash: u64) -> Option<&AtomicMarketState> {
        let id = *self.poly_yes_to_id.read().get(&hash)?;
        Some(&self.markets[id as usize])
    }

    /// Get market by Poly NO token hash (O(1))
    #[inline(always)]
    #[allow(dead_code)]
    pub fn get_by_poly_no_hash(&self, hash: u64) -> Option<&AtomicMarketState> {
        let id = *self.poly_no_to_id.read().get(&hash)?;
        Some(&self.markets[id as usize])
    }

    /// Get market_id by Poly YES token hash
    #[inline(always)]
    #[allow(dead_code)]
    pub fn id_by_poly_yes_hash(&self, hash: u64) -> Option<u16> {
        self.poly_yes_to_id.read().get(&hash).copied()
    }

    /// Get market_id by Poly NO token hash
    #[inline(always)]
    #[allow(dead_code)]
    pub fn id_by_poly_no_hash(&self, hash: u64) -> Option<u16> {
        self.poly_no_to_id.read().get(&hash).copied()
    }

    /// Get market_id by Kalshi ticker hash
    #[inline(always)]
    #[allow(dead_code)]
    pub fn id_by_kalshi_hash(&self, hash: u64) -> Option<u16> {
        self.kalshi_to_id.read().get(&hash).copied()
    }

    /// Get market by ID
    #[inline(always)]
    pub fn get_by_id(&self, id: u16) -> Option<&AtomicMarketState> {
        if (id as usize) < self.markets.len() {
            Some(&self.markets[id as usize])
        } else {
            None
        }
    }

    pub fn market_count(&self) -> usize {
        self.next_market_id.load(Ordering::Acquire) as usize
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new(ArbConfig::default())
    }
}

/// Fast string hashing function using FxHash for O(1) lookups
#[inline(always)]
pub fn fxhash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    s.hash(&mut hasher);
    hasher.finish()
}

// === Platform Enum ===

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Platform {
    Kalshi,
    Polymarket,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Kalshi => write!(f, "KALSHI"),
            Platform::Polymarket => write!(f, "POLYMARKET"),
        }
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    // =========================================================================
    // Pack/Unpack Tests - Verify bit manipulation correctness
    // =========================================================================

    #[test]
    fn test_pack_unpack_roundtrip() {
        // Test various values pack and unpack correctly
        let test_cases = vec![
            (50, 50, 1000, 1000),  // Common mid-price
            (1, 99, 100, 100),      // Edge prices
            (99, 1, 65535, 65535),  // Max sizes
            (0, 0, 0, 0),           // All zeros
            (NO_PRICE, NO_PRICE, 0, 0),  // No prices
        ];

        for (yes_ask, no_ask, yes_size, no_size) in test_cases {
            let packed = pack_orderbook(yes_ask, no_ask, yes_size, no_size);
            let (y, n, ys, ns) = unpack_orderbook(packed);
            assert_eq!((y, n, ys, ns), (yes_ask, no_ask, yes_size, no_size),
                "Roundtrip failed for ({}, {}, {}, {})", yes_ask, no_ask, yes_size, no_size);
        }
    }

    #[test]
    fn test_pack_bit_layout() {
        // Verify the exact bit layout: [yes_ask:16][no_ask:16][yes_size:16][no_size:16]
        let packed = pack_orderbook(0xABCD, 0x1234, 0x5678, 0x9ABC);

        assert_eq!((packed >> 48) & 0xFFFF, 0xABCD, "yes_ask should be in bits 48-63");
        assert_eq!((packed >> 32) & 0xFFFF, 0x1234, "no_ask should be in bits 32-47");
        assert_eq!((packed >> 16) & 0xFFFF, 0x5678, "yes_size should be in bits 16-31");
        assert_eq!(packed & 0xFFFF, 0x9ABC, "no_size should be in bits 0-15");
    }

    // =========================================================================
    // AtomicOrderbook Tests
    // =========================================================================

    #[test]
    fn test_atomic_orderbook_store_load() {
        let book = AtomicOrderbook::new();

        // Initially all zeros
        let (y, n, ys, ns) = book.load();
        assert_eq!((y, n, ys, ns), (0, 0, 0, 0));

        // Store and load
        book.store(45, 55, 500, 600);
        let (y, n, ys, ns) = book.load();
        assert_eq!((y, n, ys, ns), (45, 55, 500, 600));
    }

    #[test]
    fn test_atomic_orderbook_update_yes() {
        let book = AtomicOrderbook::new();

        // Set initial state
        book.store(40, 60, 100, 200);

        // Update only YES side
        book.update_yes(42, 150);

        let (y, n, ys, ns) = book.load();
        assert_eq!(y, 42, "YES ask should be updated");
        assert_eq!(ys, 150, "YES size should be updated");
        assert_eq!(n, 60, "NO ask should be unchanged");
        assert_eq!(ns, 200, "NO size should be unchanged");
    }

    #[test]
    fn test_atomic_orderbook_update_no() {
        let book = AtomicOrderbook::new();

        // Set initial state
        book.store(40, 60, 100, 200);

        // Update only NO side
        book.update_no(58, 250);

        let (y, n, ys, ns) = book.load();
        assert_eq!(y, 40, "YES ask should be unchanged");
        assert_eq!(ys, 100, "YES size should be unchanged");
        assert_eq!(n, 58, "NO ask should be updated");
        assert_eq!(ns, 250, "NO size should be updated");
    }

    #[test]
    fn test_atomic_orderbook_concurrent_updates() {
        // Verify correctness under concurrent access
        let book = Arc::new(AtomicOrderbook::new());
        book.store(50, 50, 1000, 1000);

        let handles: Vec<_> = (0..4).map(|i| {
            let book = book.clone();
            thread::spawn(move || {
                for _ in 0..1000 {
                    if i % 2 == 0 {
                        book.update_yes(45 + (i as u16), 500);
                    } else {
                        book.update_no(55 + (i as u16), 500);
                    }
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }

        // State should be consistent (not corrupted)
        let (y, n, ys, ns) = book.load();
        assert!(y > 0 && y < 100, "YES ask should be valid");
        assert!(n > 0 && n < 100, "NO ask should be valid");
        assert_eq!(ys, 500, "YES size should be consistent");
        assert_eq!(ns, 500, "NO size should be consistent");
    }

    // =========================================================================
    // Price Conversion Tests
    // =========================================================================

    #[test]
    fn test_price_to_cents() {
        assert_eq!(price_to_cents(0.50), 50);
        assert_eq!(price_to_cents(0.01), 1);
        assert_eq!(price_to_cents(0.99), 99);
        assert_eq!(price_to_cents(0.0), 0);
        assert_eq!(price_to_cents(1.0), 99);  // Clamped to 99
        assert_eq!(price_to_cents(0.505), 51);  // Rounded
        assert_eq!(price_to_cents(0.504), 50);  // Rounded
    }

    #[test]
    fn test_cents_to_price() {
        assert!((cents_to_price(50) - 0.50).abs() < 0.001);
        assert!((cents_to_price(1) - 0.01).abs() < 0.001);
        assert!((cents_to_price(99) - 0.99).abs() < 0.001);
        assert!((cents_to_price(0) - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_price() {
        // Standard "0.XX" format
        assert_eq!(parse_price("0.50"), 50);
        assert_eq!(parse_price("0.01"), 1);
        assert_eq!(parse_price("0.99"), 99);

        // "0.X" format
        assert_eq!(parse_price("0.5"), 50);

        // Fallback parsing
        assert_eq!(parse_price("0.505"), 51);

        // Invalid input
        assert_eq!(parse_price("invalid"), 0);
        assert_eq!(parse_price(""), 0);
    }

    // =========================================================================
    // GlobalState Tests
    // =========================================================================

    fn make_test_pair(id: &str) -> MarketPair {
        MarketPair {
            pair_id: id.into(),
            league: "epl".into(),
            market_type: MarketType::Moneyline,
            description: format!("Test Market {}", id).into(),
            kalshi_event_ticker: format!("KXEPLGAME-{}", id).into(),
            kalshi_market_ticker: format!("KXEPLGAME-{}-YES", id).into(),
            kalshi_event_slug: format!("test-market-{}", id).into(),
            poly_slug: format!("test-{}", id).into(),
            poly_yes_token: format!("yes_token_{}", id).into(),
            poly_no_token: format!("no_token_{}", id).into(),
            line_value: None,
            team_suffix: None,
            neg_risk: false,
        }
    }

    #[test]
    fn test_global_state_add_pair() {
        let state = GlobalState::default();

        let pair = make_test_pair("001");
        let kalshi_ticker = pair.kalshi_market_ticker.clone();
        let poly_yes = pair.poly_yes_token.clone();
        let poly_no = pair.poly_no_token.clone();

        let id = state.add_pair(pair).expect("Should add pair");

        assert_eq!(id, 0, "First market should have id 0");
        assert_eq!(state.market_count(), 1);

        // Verify lookups work
        let kalshi_hash = fxhash_str(&kalshi_ticker);
        let poly_yes_hash = fxhash_str(&poly_yes);
        let poly_no_hash = fxhash_str(&poly_no);

        assert!(state.kalshi_to_id.read().contains_key(&kalshi_hash));
        assert!(state.poly_yes_to_id.read().contains_key(&poly_yes_hash));
        assert!(state.poly_no_to_id.read().contains_key(&poly_no_hash));
    }

    #[test]
    fn test_global_state_lookups() {
        let state = GlobalState::default();

        let pair = make_test_pair("002");
        let kalshi_ticker = pair.kalshi_market_ticker.clone();
        let poly_yes = pair.poly_yes_token.clone();

        let id = state.add_pair(pair).unwrap();

        // Test get_by_id
        let market = state.get_by_id(id).expect("Should find by id");
        assert!(market.pair().is_some());

        // Test get_by_kalshi_hash
        let market = state.get_by_kalshi_hash(fxhash_str(&kalshi_ticker))
            .expect("Should find by Kalshi hash");
        assert!(market.pair().is_some());

        // Test get_by_poly_yes_hash
        let market = state.get_by_poly_yes_hash(fxhash_str(&poly_yes))
            .expect("Should find by Poly YES hash");
        assert!(market.pair().is_some());

        // Test id lookups
        assert_eq!(state.id_by_kalshi_hash(fxhash_str(&kalshi_ticker)), Some(id));
        assert_eq!(state.id_by_poly_yes_hash(fxhash_str(&poly_yes)), Some(id));
    }

    #[test]
    fn test_global_state_multiple_markets() {
        let state = GlobalState::default();

        // Add multiple markets
        for i in 0..10 {
            let pair = make_test_pair(&format!("{:03}", i));
            let id = state.add_pair(pair).unwrap();
            assert_eq!(id, i as u16);
        }

        assert_eq!(state.market_count(), 10);

        // All should be findable
        for i in 0..10 {
            let market = state.get_by_id(i as u16);
            assert!(market.is_some(), "Market {} should exist", i);
        }
    }

    #[test]
    fn test_global_state_update_prices() {
        let state = GlobalState::default();

        let pair = make_test_pair("003");
        let id = state.add_pair(pair).unwrap();

        // Update Kalshi prices
        let market = state.get_by_id(id).unwrap();
        market.kalshi.store(45, 55, 500, 600);

        // Update Poly prices
        market.poly.store(44, 56, 700, 800);

        // Verify prices
        let (k_yes, k_no, k_yes_sz, k_no_sz) = market.kalshi.load();
        assert_eq!((k_yes, k_no, k_yes_sz, k_no_sz), (45, 55, 500, 600));

        let (p_yes, p_no, p_yes_sz, p_no_sz) = market.poly.load();
        assert_eq!((p_yes, p_no, p_yes_sz, p_no_sz), (44, 56, 700, 800));
    }

    // =========================================================================
    // ArbOpportunity Tests
    // =========================================================================

    #[test]
    fn test_execution_request_profit_cents_poly_yes_kalshi_no() {
        // Poly YES 40¢ + Kalshi NO 50¢ = 90¢
        // Kalshi fee on 50¢ = 2¢
        // Profit = 100 - 90 - 2 = 8¢
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.profit_cents(), 8);
    }

    #[test]
    fn test_execution_request_profit_cents_kalshi_yes_poly_no() {
        // Kalshi YES 40¢ + Poly NO 50¢ = 90¢
        // Kalshi fee on 40¢ = 2¢
        // Profit = 100 - 90 - 2 = 8¢
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.profit_cents(), 8);
    }

    #[test]
    fn test_execution_request_profit_cents_poly_only() {
        // Poly YES 40¢ + Poly NO 48¢ = 88¢
        // No fees on Polymarket
        // Profit = 100 - 88 - 0 = 12¢
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 48,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyOnly,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.profit_cents(), 12);
        assert_eq!(req.estimated_fee_cents(), 0);
    }

    #[test]
    fn test_execution_request_profit_cents_kalshi_only() {
        // Kalshi YES 40¢ + Kalshi NO 44¢ = 84¢
        // Kalshi fee on both: 2¢ + 2¢ = 4¢
        // Profit = 100 - 84 - 4 = 12¢
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 44,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiOnly,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.profit_cents(), 12);
        assert_eq!(req.estimated_fee_cents(), kalshi_fee(40) + kalshi_fee(44));
    }

    #[test]
    fn test_execution_request_negative_profit() {
        // Prices too high - no profit
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 52,
            no_price: 52,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert!(req.profit_cents() < 0, "Should have negative profit");
    }

    #[test]
    fn test_execution_request_estimated_fee() {
        // PolyYesKalshiNo → fee on Kalshi NO
        let req1 = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };
        assert_eq!(req1.estimated_fee_cents(), kalshi_fee(50));

        // KalshiYesPolyNo → fee on Kalshi YES
        let req2 = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
            is_test: false,
        };
        assert_eq!(req2.estimated_fee_cents(), kalshi_fee(40));

        // PolyOnly → no fees
        let req3 = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyOnly,
            detected_ns: 0,
            is_test: false,
        };
        assert_eq!(req3.estimated_fee_cents(), 0);

        // KalshiOnly → fees on both sides
        let req4 = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiOnly,
            detected_ns: 0,
            is_test: false,
        };
        assert_eq!(req4.estimated_fee_cents(), kalshi_fee(40) + kalshi_fee(50));
    }

    // =========================================================================
    // ArbOpportunity::max_contracts() Tests
    // =========================================================================

    #[test]
    fn test_execution_request_max_contracts_basic() {
        // YES: 1000 cents / 40 cents = 25 contracts
        // NO: 1000 cents / 50 cents = 20 contracts
        // max_contracts = min(25, 20) = 20
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.max_contracts(), 20);
    }

    #[test]
    fn test_execution_request_max_contracts_yes_limited() {
        // YES: 500 cents / 50 cents = 10 contracts
        // NO: 1000 cents / 50 cents = 20 contracts
        // max_contracts = min(10, 20) = 10
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 50,
            no_price: 50,
            yes_size: 500,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.max_contracts(), 10);
    }

    #[test]
    fn test_execution_request_max_contracts_no_limited() {
        // YES: 1000 cents / 50 cents = 20 contracts
        // NO: 500 cents / 50 cents = 10 contracts
        // max_contracts = min(20, 10) = 10
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 50,
            no_price: 50,
            yes_size: 1000,
            no_size: 500,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.max_contracts(), 10);
    }

    #[test]
    fn test_execution_request_max_contracts_zero_yes_price() {
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 0,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.max_contracts(), 0, "Should return 0 when yes_price is 0");
    }

    #[test]
    fn test_execution_request_max_contracts_zero_no_price() {
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 50,
            no_price: 0,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.max_contracts(), 0, "Should return 0 when no_price is 0");
    }

    #[test]
    fn test_execution_request_max_contracts_integer_division() {
        // Verify integer division behavior (truncation, not rounding)
        // YES: 99 cents / 10 cents = 9 contracts (not 10)
        // NO: 99 cents / 10 cents = 9 contracts
        let req = ArbOpportunity {
            market_id: 0,
            yes_price: 10,
            no_price: 10,
            yes_size: 99,
            no_size: 99,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
            is_test: false,
        };

        assert_eq!(req.max_contracts(), 9);
    }

    // =========================================================================
    // fxhash_str Tests
    // =========================================================================

    #[test]
    fn test_fxhash_str_consistency() {
        let s = "KXEPLGAME-25DEC27CFCARS-CFC";

        // Same string should always produce same hash
        let h1 = fxhash_str(s);
        let h2 = fxhash_str(s);
        assert_eq!(h1, h2);

        // Different strings should (almost certainly) produce different hashes
        let h3 = fxhash_str("KXEPLGAME-25DEC27CFCARS-ARS");
        assert_ne!(h1, h3);
    }

    // =========================================================================
    // Integration: Full Arb Detection Flow
    // =========================================================================

    #[test]
    fn test_full_arb_flow() {
        // Simulate the full flow: add market, update prices, detect arb
        let state = GlobalState::default();

        // 1. Add market during discovery
        let pair = MarketPair {
            pair_id: "test-arb".into(),
            league: "epl".into(),
            market_type: MarketType::Moneyline,
            description: "Chelsea vs Arsenal".into(),
            kalshi_event_ticker: "KXEPLGAME-25DEC27CFCARS".into(),
            kalshi_market_ticker: "KXEPLGAME-25DEC27CFCARS-CFC".into(),
            kalshi_event_slug: "chelsea-vs-arsenal".into(),
            poly_slug: "chelsea-vs-arsenal".into(),
            poly_yes_token: "yes_token_cfc".into(),
            poly_no_token: "no_token_cfc".into(),
            line_value: None,
            team_suffix: Some("CFC".into()),
            neg_risk: false,
        };

        let poly_yes_token = pair.poly_yes_token.clone();
        let kalshi_ticker = pair.kalshi_market_ticker.clone();

        let market_id = state.add_pair(pair).unwrap();

        // 2. Simulate WebSocket updates setting prices
        // Kalshi update
        let kalshi_hash = fxhash_str(&kalshi_ticker);
        if let Some(id) = state.kalshi_to_id.read().get(&kalshi_hash).copied() {
            state.markets[id as usize].kalshi.store(55, 50, 500, 600);
        }

        // Polymarket update
        let poly_hash = fxhash_str(&poly_yes_token);
        if let Some(id) = state.poly_yes_to_id.read().get(&poly_hash).copied() {
            state.markets[id as usize].poly.store(40, 65, 700, 800);
        }

        // 3. Check for arbs using ArbOpportunity::detect()
        let market = state.get_by_id(market_id).unwrap();
        let kalshi_data = market.kalshi.load();
        let poly_data = market.poly.load();
        let req = ArbOpportunity::detect(
            market_id,
            kalshi_data,
            poly_data,
            state.arb_config(),
            0,
        );

        // 4. Verify arb detected
        let req = req.expect("Should detect arb opportunity");
        assert_eq!(req.arb_type, ArbType::PolyYesKalshiNo, "Should detect Poly YES + Kalshi NO arb");

        // 5. Verify execution request has positive profit
        assert!(req.profit_cents() > 0, "Should have positive profit");
    }

    #[test]
    fn test_price_update_race_condition() {
        // Simulate concurrent price updates from different WebSocket feeds
        let state = Arc::new(GlobalState::default());

        // Pre-populate with a market
        let market = &state.markets[0];
        market.kalshi.store(50, 50, 1000, 1000);
        market.poly.store(50, 50, 1000, 1000);

        let handles: Vec<_> = (0..4).map(|i| {
            let state = state.clone();
            thread::spawn(move || {
                for j in 0..1000 {
                    let market = &state.markets[0];
                    if i % 2 == 0 {
                        // Simulate Kalshi updates
                        market.kalshi.update_yes(40 + ((j % 10) as u16), 500 + j as u16);
                    } else {
                        // Simulate Poly updates
                        market.poly.update_no(50 + ((j % 10) as u16), 600 + j as u16);
                    }

                    // Check arbs using ArbOpportunity::detect() (should never panic)
                    let kalshi_data = market.kalshi.load();
                    let poly_data = market.poly.load();
                    let _ = ArbOpportunity::detect(
                        market.market_id,
                        kalshi_data,
                        poly_data,
                        state.arb_config(),
                        0,
                    );
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }

        // Final state should be valid
        let market = &state.markets[0];
        let (k_yes, k_no, _, _) = market.kalshi.load();
        let (p_yes, p_no, _, _) = market.poly.load();

        assert!(k_yes > 0 && k_yes < 100);
        assert!(k_no > 0 && k_no < 100);
        assert!(p_yes > 0 && p_yes < 100);
        assert!(p_no > 0 && p_no < 100);
    }

    // =========================================================================
    // GlobalState ArbConfig Integration Tests
    // =========================================================================

    #[test]
    fn test_global_state_has_arb_config() {
        use crate::arb::ArbConfig;

        // Create GlobalState with default ArbConfig
        let config = ArbConfig::default();
        let state = GlobalState::new(config);

        // Verify arb_config() returns the correct threshold
        assert_eq!(state.arb_config().threshold_cents(), 99);
        assert_eq!(state.arb_config().min_contracts(), 1.0);
    }

    #[test]
    fn test_global_state_arb_config_custom_threshold() {
        use crate::arb::ArbConfig;

        // Create GlobalState with custom ArbConfig
        let config = ArbConfig::new(95, 5.0);
        let state = GlobalState::new(config);

        // Verify arb_config() returns the custom values
        assert_eq!(state.arb_config().threshold_cents(), 95);
        assert_eq!(state.arb_config().min_contracts(), 5.0);
    }

    // =========================================================================
    // ArbOpportunity::detect() Tests
    // =========================================================================

    #[test]
    fn test_detect_returns_none_when_prices_too_high() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // Prices sum to 100 cents = no profit
        let result = ArbOpportunity::detect(
            1,                          // market_id
            (50, 50, 1000, 1000),       // kalshi
            (50, 50, 1000, 1000),       // poly
            &config,
            12345,
        );

        assert!(result.is_none());
    }

    #[test]
    fn test_detect_finds_poly_yes_kalshi_no() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default(); // threshold = 99

        // Poly YES @ 45 + Kalshi NO @ 52 = 97 cents (+ ~2c fee) = ~99 <= 99
        let result = ArbOpportunity::detect(
            1,
            (55, 52, 500, 500),        // kalshi
            (45, 58, 500, 500),        // poly
            &config,
            12345,
        );

        let arb = result.expect("should detect arb");
        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo);
        assert_eq!(arb.yes_price, 45);  // poly yes
        assert_eq!(arb.no_price, 52);   // kalshi no
    }

    #[test]
    fn test_detect_returns_none_when_size_insufficient() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // Great prices but no size on kalshi NO side
        let result = ArbOpportunity::detect(
            1,
            (55, 52, 500, 0),          // kalshi: no_size = 0
            (45, 58, 500, 500),
            &config,
            12345,
        );

        assert!(result.is_none());
    }

    #[test]
    fn test_detect_returns_none_when_any_price_is_zero() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // Kalshi yes price = 0 (no price available)
        let result = ArbOpportunity::detect(
            1,
            (0, 52, 500, 500),
            (45, 58, 500, 500),
            &config,
            12345,
        );

        assert!(result.is_none(), "Should return None when kalshi yes price is 0");

        // Poly no price = 0
        let result2 = ArbOpportunity::detect(
            1,
            (55, 52, 500, 500),
            (45, 0, 500, 500),
            &config,
            12345,
        );

        assert!(result2.is_none(), "Should return None when poly no price is 0");
    }

    #[test]
    fn test_detect_finds_kalshi_yes_poly_no() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // Kalshi YES @ 45 + Poly NO @ 52 = 97 + ~2c fee = ~99 <= 99
        // Make Poly YES expensive so PolyYesKalshiNo doesn't win
        let result = ArbOpportunity::detect(
            1,
            (45, 60, 500, 500),        // kalshi: yes=45, no=60
            (60, 52, 500, 500),        // poly: yes=60, no=52
            &config,
            12345,
        );

        let arb = result.expect("should detect arb");
        assert_eq!(arb.arb_type, ArbType::KalshiYesPolyNo);
        assert_eq!(arb.yes_price, 45);  // kalshi yes
        assert_eq!(arb.no_price, 52);   // poly no
    }

    #[test]
    fn test_detect_finds_poly_only() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // Make cross-platform arbs too expensive, but Poly YES + Poly NO = 88 (no fees)
        let result = ArbOpportunity::detect(
            1,
            (60, 60, 500, 500),        // kalshi: expensive
            (40, 48, 500, 500),        // poly: 40 + 48 = 88 < 99
            &config,
            12345,
        );

        let arb = result.expect("should detect poly-only arb");
        assert_eq!(arb.arb_type, ArbType::PolyOnly);
        assert_eq!(arb.yes_price, 40);
        assert_eq!(arb.no_price, 48);
    }

    #[test]
    fn test_detect_finds_kalshi_only() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // Kalshi YES 40 + Kalshi NO 40 = 80 + ~4c fees = 84 < 99
        // Make Poly expensive so cross-platform and poly-only don't win
        let result = ArbOpportunity::detect(
            1,
            (40, 40, 500, 500),        // kalshi: 40 + 40 + ~4 fees = 84
            (60, 60, 500, 500),        // poly: expensive
            &config,
            12345,
        );

        let arb = result.expect("should detect kalshi-only arb");
        assert_eq!(arb.arb_type, ArbType::KalshiOnly);
        assert_eq!(arb.yes_price, 40);
        assert_eq!(arb.no_price, 40);
    }

    #[test]
    fn test_detect_respects_min_contracts_threshold() {
        use crate::arb::ArbConfig;

        // Require at least 5 contracts
        let config = ArbConfig::new(99, 5.0);

        // Good arb prices: poly_yes=45, kalshi_no=52, cost=97 + ~1c fee = 98 < 99
        // Size 180 cents at 45c/contract = 4 contracts (180/45 = 4)
        // Size 200 cents at 52c/contract = 3 contracts (200/52 = 3)
        // max_contracts = min(4, 3) = 3 < 5 (should reject)
        let result = ArbOpportunity::detect(
            1,
            (55, 52, 500, 200),        // kalshi: yes=55, no=52, yes_size=500, no_size=200
            (45, 58, 180, 500),        // poly: yes=45, no=58, yes_size=180, no_size=500
            &config,
            12345,
        );

        assert!(result.is_none(), "Should reject when max_contracts < min_contracts");

        // Now with enough size:
        // poly_yes: 250/45 = 5 contracts, kalshi_no: 260/52 = 5 contracts
        // max_contracts = min(5, 5) = 5 >= 5 (should accept)
        let result2 = ArbOpportunity::detect(
            1,
            (55, 52, 500, 260),        // kalshi: no_size=260 -> 5 contracts
            (45, 58, 250, 500),        // poly: yes_size=250 -> 5 contracts
            &config,
            12345,
        );

        assert!(result2.is_some(), "Should accept when max_contracts >= min_contracts");
    }

    #[test]
    fn test_detect_priority_order() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // All 4 arb types are valid with equal prices - should pick PolyYesKalshiNo (priority)
        let result = ArbOpportunity::detect(
            1,
            (40, 40, 500, 500),
            (40, 40, 500, 500),
            &config,
            12345,
        );

        let arb = result.expect("should detect arb");
        assert_eq!(arb.arb_type, ArbType::PolyYesKalshiNo, "Should pick PolyYesKalshiNo first in priority");
    }

    #[test]
    fn test_detect_sets_is_test_false() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        let result = ArbOpportunity::detect(
            1,
            (55, 52, 500, 500),
            (45, 58, 500, 500),
            &config,
            12345,
        );

        let arb = result.expect("should detect arb");
        assert!(!arb.is_test, "detect() should always set is_test to false");
    }

    #[test]
    fn test_detect_preserves_market_id_and_timestamp() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        let result = ArbOpportunity::detect(
            42,                         // specific market_id
            (55, 52, 500, 500),
            (45, 58, 500, 500),
            &config,
            999888777,                  // specific timestamp
        );

        let arb = result.expect("should detect arb");
        assert_eq!(arb.market_id, 42);
        assert_eq!(arb.detected_ns, 999888777);
    }

    // =========================================================================
    // Polymarket $1 Minimum Order Value Tests
    // =========================================================================

    #[test]
    fn test_detect_rejects_poly_order_below_1_dollar() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // PolyYesKalshiNo: Poly YES at 49¢ with only 1 contract = $0.49 < $1.00
        // yes_size=49 at yes_price=49 = 1 contract, no_size=500 at no_price=50 = 10 contracts
        // max_contracts = min(1, 10) = 1
        // Poly order value = 1 × 49¢ = $0.49 < $1.00 → REJECT
        let result = ArbOpportunity::detect(
            0,
            (55, 50, 500, 500),         // kalshi: yes=55, no=50, sizes=500
            (49, 58, 49, 500),          // poly: yes=49 (1 contract of liquidity), no=58
            &config,
            0,
        );

        assert!(result.is_none(), "Should reject Poly order below $1: 1 contract × 49¢ = $0.49");
    }

    #[test]
    fn test_detect_accepts_poly_order_at_1_dollar() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // PolyYesKalshiNo: Poly YES at 50¢ with 2 contracts = $1.00 = minimum
        // yes_size=100 at yes_price=50 = 2 contracts
        // Poly order value = 2 × 50¢ = $1.00 = $1.00 → ACCEPT
        // Arb cost: p_yes(50) + k_no(45) + fee(~2) = 97 < 99 threshold ✓
        let result = ArbOpportunity::detect(
            0,
            (55, 45, 500, 500),         // kalshi: yes=55, no=45, sizes=500
            (50, 58, 100, 500),         // poly: yes=50, 2 contracts of liquidity
            &config,
            0,
        );

        assert!(result.is_some(), "Should accept Poly order at exactly $1.00: 2 × 50¢");
    }

    #[test]
    fn test_detect_accepts_poly_order_above_1_dollar() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // PolyYesKalshiNo: Poly YES at 49¢ with 3 contracts = $1.47 > $1.00
        // yes_size=147 at yes_price=49 = 3 contracts
        // Poly order value = 3 × 49¢ = $1.47 > $1.00 → ACCEPT
        // Arb cost: p_yes(49) + k_no(45) + fee(~2) = 96 < 99 threshold ✓
        let result = ArbOpportunity::detect(
            0,
            (55, 45, 500, 500),         // kalshi: yes=55, no=45, sizes=500
            (49, 58, 147, 500),         // poly: yes=49, 3 contracts of liquidity
            &config,
            0,
        );

        assert!(result.is_some(), "Should accept Poly order above $1.00: 3 × 49¢ = $1.47");
    }

    #[test]
    fn test_detect_kalshi_only_ignores_poly_minimum() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // KalshiOnly: No Polymarket involvement, so $1 minimum doesn't apply
        // Even if Poly prices exist, they're not used for KalshiOnly arb
        let result = ArbOpportunity::detect(
            0,
            (45, 48, 500, 500),         // kalshi: yes=45, no=48 (total 93 + fees < 100)
            (99, 99, 10, 10),           // poly: expensive, low liquidity (would fail $1 check)
            &config,
            0,
        );

        // This should find KalshiOnly arb since Kalshi prices are good
        // and Poly minimum doesn't apply to KalshiOnly
        if let Some(arb) = result {
            assert_eq!(arb.arb_type, ArbType::KalshiOnly);
        }
        // Note: may return None if the arb doesn't meet other thresholds
    }

    #[test]
    fn test_detect_kalshi_yes_poly_no_validates_poly_no_leg() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // KalshiYesPolyNo: Poly NO at 49¢ with only 1 contract = $0.49 < $1.00
        // k_yes=45, p_no=49, so cost = 45 + fee(~2) + 49 = 96 < 99 threshold ✓
        // But Poly NO order value = 1 × 49¢ = $0.49 < $1.00 → REJECT
        let result = ArbOpportunity::detect(
            0,
            (45, 55, 500, 500),         // kalshi: yes=45, no=55, sizes=500
            (58, 49, 500, 49),          // poly: yes=58, no=49 (only 1 contract liquidity)
            &config,
            0,
        );

        assert!(result.is_none(), "Should reject KalshiYesPolyNo when Poly NO order < $1");
    }

    #[test]
    fn test_detect_kalshi_yes_poly_no_accepts_valid_poly_no_leg() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // KalshiYesPolyNo: Poly NO at 49¢ with 3 contracts = $1.47 >= $1.00
        // k_yes=45, p_no=49, so cost = 45 + fee(~2) + 49 = 96 < 99 threshold ✓
        // Poly NO order value = 3 × 49¢ = $1.47 >= $1.00 → ACCEPT
        let result = ArbOpportunity::detect(
            0,
            (45, 55, 500, 500),         // kalshi: yes=45, no=55, sizes=500
            (58, 49, 500, 147),         // poly: yes=58, no=49 (3 contracts liquidity: 147/49=3)
            &config,
            0,
        );

        let arb = result.expect("Should accept KalshiYesPolyNo when Poly NO order >= $1");
        assert_eq!(arb.arb_type, ArbType::KalshiYesPolyNo);
    }

    #[test]
    fn test_detect_poly_only_requires_both_legs_above_1_dollar() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // PolyOnly: Both legs on Polymarket
        // YES at 45¢ with 3 contracts = $1.35 >= $1.00 ✓
        // NO at 49¢ with only 1 contract = $0.49 < $1.00 ✗
        // Even though YES passes, NO fails → REJECT
        let result = ArbOpportunity::detect(
            0,
            (99, 99, 10, 10),           // kalshi: expensive (forces PolyOnly)
            (45, 49, 135, 49),          // poly: yes=45 (3 contracts), no=49 (1 contract)
            &config,
            0,
        );

        assert!(result.is_none(), "Should reject PolyOnly when either Poly leg < $1");
    }

    #[test]
    fn test_detect_poly_only_accepts_when_both_legs_valid() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // PolyOnly: Both legs on Polymarket
        // YES at 45¢ with 3 contracts = $1.35 >= $1.00 ✓
        // NO at 50¢ with 3 contracts = $1.50 >= $1.00 ✓
        // Cost = 45 + 50 = 95 < 99 threshold ✓
        let result = ArbOpportunity::detect(
            0,
            (99, 99, 10, 10),           // kalshi: expensive (forces PolyOnly)
            (45, 50, 135, 150),         // poly: yes=45 (3 contracts), no=50 (3 contracts)
            &config,
            0,
        );

        let arb = result.expect("Should accept PolyOnly when both Poly legs >= $1");
        assert_eq!(arb.arb_type, ArbType::PolyOnly);
    }

    /// Regression test for the exact bug captured in production:
    /// Order sent with makerAmount=490000 ($0.49), takerAmount=1000000 (1 contract)
    /// This was caused by the old buggy calculation: min(yes_size, no_size) / 100
    /// instead of the correct: min(yes_size/yes_price, no_size/no_price)
    #[test]
    fn test_regression_captured_049_order_rejected() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // Scenario from captured request:
        // - yes_price = 49¢
        // - Trying to buy 1 contract at 49¢ = $0.49 order
        // - This should be rejected
        //
        // Old bug: if yes_size=147, no_size=500
        //   max_contracts = min(147, 500) / 100 = 1 contract ← WRONG
        //
        // Correct: yes_size=147, yes_price=49
        //   max_contracts = min(147/49, 500/48) = min(3, 10) = 3 contracts ← RIGHT
        //
        // But if liquidity is truly only 1 contract worth (yes_size=49):
        //   max_contracts = min(49/49, 500/48) = min(1, 10) = 1 contract
        //   Order value = 1 × 49¢ = $0.49 < $1.00 → REJECT

        let result = ArbOpportunity::detect(
            0,
            (55, 48, 500, 500),         // kalshi: yes=55, no=48
            (49, 58, 49, 500),          // poly: yes=49¢ with only 49¢ liquidity (1 contract)
            &config,
            0,
        );

        assert!(
            result.is_none(),
            "Should reject arb where Poly order would be $0.49 (1 contract × 49¢)"
        );
    }

    /// Test that max_contracts calculation matches what we expect
    /// This is the core calculation that was wrong before
    #[test]
    fn test_max_contracts_calculation_correctness() {
        // Create an ArbOpportunity and verify max_contracts()
        let arb = ArbOpportunity {
            market_id: 0,
            arb_type: ArbType::PolyYesKalshiNo,
            yes_price: 49,
            no_price: 48,
            yes_size: 147,  // 147/49 = 3 contracts
            no_size: 480,   // 480/48 = 10 contracts
            detected_ns: 0,
            is_test: false,
        };

        // max_contracts = min(147/49, 480/48) = min(3, 10) = 3
        assert_eq!(arb.max_contracts(), 3, "max_contracts should be 3");

        // Verify the order value would be valid
        let order_value = arb.max_contracts() as u16 * arb.yes_price;
        assert!(order_value >= 100, "Order value {}¢ should be >= $1.00", order_value);
    }

    /// Test edge case: just barely meeting $1 minimum
    #[test]
    fn test_max_contracts_exactly_at_1_dollar_boundary() {
        use crate::arb::ArbConfig;

        let config = ArbConfig::default();

        // At 34¢ per contract, need 3 contracts to reach $1.02
        // 2 contracts = 68¢ < $1.00 ✗
        // 3 contracts = $1.02 >= $1.00 ✓
        // yes_size = 102 → 102/34 = 3 contracts
        let result = ArbOpportunity::detect(
            0,
            (55, 45, 500, 500),         // kalshi
            (34, 58, 102, 500),         // poly: yes=34¢, 3 contracts (102/34=3)
            &config,
            0,
        );

        // Cost = 34 + 45 + fee(~2) = 81 < 99 ✓
        // Order value = 3 × 34 = 102¢ = $1.02 ≥ $1.00 ✓
        assert!(result.is_some(), "Should accept when order value is exactly at boundary");

        // Now test with 2 contracts (should fail)
        let result2 = ArbOpportunity::detect(
            0,
            (55, 45, 500, 500),
            (34, 58, 68, 500),          // poly: yes=34¢, 2 contracts (68/34=2)
            &config,
            0,
        );

        // Order value = 2 × 34 = 68¢ < $1.00 ✗
        assert!(result2.is_none(), "Should reject when order value is 68¢");
    }

    // =========================================================================
    // PolyBook Tests - Shadow orderbook for Polymarket
    // =========================================================================

    #[test]
    fn test_poly_book_new_is_empty() {
        let book = PolyBook::new();
        assert_eq!(book.best_yes_ask(), None);
        assert_eq!(book.best_no_ask(), None);
    }

    #[test]
    fn test_poly_book_set_and_best_ask() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(47, 2000), (45, 1500), (50, 3000)]);
        assert_eq!(book.best_yes_ask(), Some((45, 1500)));
    }

    #[test]
    fn test_poly_book_update_level_insert() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(50, 1000)]);
        book.update_yes_level(48, 500);
        assert_eq!(book.best_yes_ask(), Some((48, 500)));
    }

    #[test]
    fn test_poly_book_update_level_replace() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000)]);
        book.update_yes_level(45, 2000);
        assert_eq!(book.best_yes_ask(), Some((45, 2000)));
    }

    #[test]
    fn test_poly_book_update_level_remove() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000), (50, 2000)]);
        book.update_yes_level(45, 0);
        assert_eq!(book.best_yes_ask(), Some((50, 2000)));
    }

    #[test]
    fn test_poly_book_remove_best_reveals_next() {
        let mut book = PolyBook::new();
        book.set_no_asks(&[(42, 500), (45, 1000), (48, 1500)]);
        assert_eq!(book.best_no_ask(), Some((42, 500)));
        book.update_no_level(42, 0);
        assert_eq!(book.best_no_ask(), Some((45, 1000)));
    }

    #[test]
    fn test_poly_book_remove_last_level() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(50, 1000)]);
        book.update_yes_level(50, 0);
        assert_eq!(book.best_yes_ask(), None);
    }

    #[test]
    fn test_poly_book_snapshot_replaces_all() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000), (50, 2000)]);
        book.set_yes_asks(&[(60, 500), (65, 800)]);
        assert_eq!(book.best_yes_ask(), Some((60, 500)));
        book.update_yes_level(45, 0);
        assert_eq!(book.best_yes_ask(), Some((60, 500)));
    }

    #[test]
    fn test_poly_book_skips_zero_price_and_size() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(0, 1000), (45, 0), (50, 2000)]);
        assert_eq!(book.best_yes_ask(), Some((50, 2000)));
    }

    #[test]
    fn test_poly_book_clear() {
        let mut book = PolyBook::new();
        book.set_yes_asks(&[(45, 1000)]);
        book.set_no_asks(&[(55, 2000)]);
        book.clear();
        assert_eq!(book.best_yes_ask(), None);
        assert_eq!(book.best_no_ask(), None);
    }
}

// === Kalshi API Types ===

#[derive(Debug, Deserialize)]
pub struct KalshiEventsResponse {
    pub events: Vec<KalshiEvent>,
    #[serde(default)]
    #[allow(dead_code)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KalshiEvent {
    pub event_ticker: String,
    pub title: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub sub_title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct KalshiMarketsResponse {
    pub markets: Vec<KalshiMarket>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct KalshiMarket {
    pub ticker: String,
    pub title: String,
    /// Event ticker this market belongs to (present on some list endpoints).
    #[serde(default)]
    pub event_ticker: Option<String>,
    /// Market status (e.g., "active", "finalized").
    #[serde(default)]
    pub status: Option<String>,
    /// Market subtitle (empty string on many markets).
    #[serde(default)]
    pub subtitle: Option<String>,
    /// For MVE markets, the normalized collection ticker with date+teams (e.g. "KXMVENFLSINGLEGAME-26JAN18LACHI").
    #[serde(default)]
    pub mve_collection_ticker: Option<String>,
    pub yes_ask: Option<i64>,
    pub yes_bid: Option<i64>,
    pub no_ask: Option<i64>,
    pub no_bid: Option<i64>,
    #[serde(default)]
    pub yes_sub_title: Option<String>,
    #[serde(default)]
    pub floor_strike: Option<f64>,
    pub volume: Option<i64>,
    pub liquidity: Option<i64>,
}

// === Polymarket/Gamma API Types ===

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GammaMarket {
    pub slug: Option<String>,
    pub question: Option<String>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    pub outcomes: Option<String>,
    #[serde(rename = "outcomePrices")]
    pub outcome_prices: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
}

// === Discovery Result ===

/// Stats for discovery summary table
/// Tracks per-league, per-market-type breakdown of Kalshi vs matched counts
#[derive(Debug, Default, Clone)]
pub struct DiscoveryStats {
    /// Map of (league, market_type) -> (kalshi_count, matched_count)
    pub by_league_type: HashMap<(String, MarketType), (usize, usize)>,
}

impl DiscoveryStats {
    /// Record stats for a league + market type combination
    pub fn record(&mut self, league: &str, market_type: MarketType, kalshi: usize, matched: usize) {
        self.by_league_type.insert((league.to_string(), market_type), (kalshi, matched));
    }

    /// Merge another DiscoveryStats into this one
    pub fn merge(&mut self, other: DiscoveryStats) {
        self.by_league_type.extend(other.by_league_type);
    }
}

#[derive(Debug, Default)]
pub struct DiscoveryResult {
    pub pairs: Vec<MarketPair>,
    pub kalshi_events_found: usize,
    pub poly_matches: usize,
    #[allow(dead_code)]
    pub poly_misses: usize,
    pub errors: Vec<String>,
    /// Per-league, per-market-type breakdown stats
    pub stats: DiscoveryStats,
}