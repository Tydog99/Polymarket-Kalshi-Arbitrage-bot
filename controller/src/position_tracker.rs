//! Position tracking and P&L calculation system.
//!
//! This module tracks all positions across both platforms, calculates cost basis,
//! and maintains real-time profit and loss calculations.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

// =============================================================================
// TRADE HISTORY TYPES
// =============================================================================

/// Reason for a trade attempt (for audit trail)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradeReason {
    /// Initial YES leg of an arbitrage
    ArbLegYes,
    /// Initial NO leg of an arbitrage
    ArbLegNo,
    /// Automatic position unwind due to leg mismatch
    AutoClose,
    /// Retry attempt during auto-close (at lower price)
    AutoCloseRetry,
    /// Blocked by circuit breaker before exchange
    CircuitBreakerBlock,
    /// User-initiated close (future use)
    ManualClose,
}

/// Status of a trade attempt
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradeStatus {
    /// Trade completed successfully (full fill)
    Success,
    /// Trade failed completely
    Failed,
    /// Trade partially filled
    PartialFill,
    /// Trade blocked before reaching exchange
    Blocked,
}

/// A single trade attempt record for audit trail
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    /// Sequence number within this position (for ordering)
    pub sequence: u32,
    /// ISO 8601 timestamp of the trade attempt
    pub timestamp: String,
    /// Why this trade was attempted
    pub reason: TradeReason,
    /// Platform: "kalshi" or "polymarket"
    pub platform: String,
    /// Side: "yes" or "no"
    pub side: String,
    /// Number of contracts requested (negative for sells/closes)
    pub requested_contracts: f64,
    /// Number of contracts actually filled (negative for sells/closes)
    pub filled_contracts: f64,
    /// Price per contract (0.0 to 1.0)
    pub price: f64,
    /// Outcome of the trade attempt
    pub status: TradeStatus,
    /// If failed/blocked, the reason why
    pub failure_reason: Option<String>,
    /// Exchange order ID (if available)
    pub order_id: Option<String>,
}

const POSITION_FILE: &str = "positions.json";

/// A single position leg on one platform
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PositionLeg {
    /// Number of contracts held
    pub contracts: f64,
    /// Total cost paid (in dollars)
    pub cost_basis: f64,
    /// Average price per contract
    pub avg_price: f64,
}

#[allow(dead_code)]
impl PositionLeg {
    pub fn add(&mut self, contracts: f64, price: f64) {
        // Only accumulate cost_basis on buys (positive contracts)
        // cost_basis represents total cost paid, never reduced on sells
        if contracts > 0.0 {
            let new_cost = contracts * price;
            self.cost_basis += new_cost;
            // Update avg_price based on new weighted average
            let new_contracts = self.contracts + contracts;
            if new_contracts > 0.0 {
                self.avg_price = self.cost_basis / new_contracts;
            }
        }
        self.contracts += contracts;
    }
    
    /// Unrealized P&L based on current market price
    pub fn unrealized_pnl(&self, current_price: f64) -> f64 {
        let current_value = self.contracts * current_price;
        current_value - self.cost_basis
    }
    
    /// Value if this position wins (pays $1 per contract)
    pub fn value_if_win(&self) -> f64 {
        self.contracts * 1.0
    }
    
    /// Profit if this position wins
    pub fn profit_if_win(&self) -> f64 {
        self.value_if_win() - self.cost_basis
    }
}

/// A paired position (arb position spans both platforms)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArbPosition {
    /// Market identifier (Kalshi ticker)
    pub market_id: String,

    /// Description for logging
    pub description: String,

    /// Kalshi YES position
    pub kalshi_yes: PositionLeg,

    /// Kalshi NO position
    pub kalshi_no: PositionLeg,

    /// Polymarket YES position
    pub poly_yes: PositionLeg,

    /// Polymarket NO position
    pub poly_no: PositionLeg,

    /// Total fees paid (Kalshi fees)
    pub total_fees: f64,

    /// Timestamp when position was opened
    pub opened_at: String,

    /// Status: "open", "closed", "resolved"
    pub status: String,

    /// Realized P&L (set when position closes/resolves)
    pub realized_pnl: Option<f64>,

    /// Complete trade history for audit trail
    #[serde(default)]
    pub trades: Vec<TradeRecord>,

    /// Internal counter for trade sequence numbers
    #[serde(default)]
    next_sequence: u32,
}

#[allow(dead_code)]
impl ArbPosition {
    pub fn new(market_id: &str, description: &str) -> Self {
        Self {
            market_id: market_id.to_string(),
            description: description.to_string(),
            status: "open".to_string(),
            opened_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        }
    }

    /// Record a trade in the history and return the sequence number assigned.
    pub fn record_trade(
        &mut self,
        reason: TradeReason,
        platform: &str,
        side: &str,
        requested_contracts: f64,
        filled_contracts: f64,
        price: f64,
        status: TradeStatus,
        failure_reason: Option<String>,
        order_id: Option<String>,
    ) -> u32 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;

        let trade = TradeRecord {
            sequence,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            reason,
            platform: platform.to_string(),
            side: side.to_string(),
            requested_contracts,
            filled_contracts,
            price,
            status,
            failure_reason,
            order_id,
        };

        self.trades.push(trade);
        sequence
    }
    
    /// Total contracts across all legs
    pub fn total_contracts(&self) -> f64 {
        self.kalshi_yes.contracts + self.kalshi_no.contracts +
        self.poly_yes.contracts + self.poly_no.contracts
    }
    
    /// Total cost basis across all legs
    pub fn total_cost(&self) -> f64 {
        self.kalshi_yes.cost_basis + self.kalshi_no.cost_basis +
        self.poly_yes.cost_basis + self.poly_no.cost_basis +
        self.total_fees
    }
    
    /// For a proper arb (YES on one platform + NO on other), one side always wins
    /// This calculates the guaranteed profit assuming the arb is balanced
    pub fn guaranteed_profit(&self) -> f64 {
        // In a balanced arb: we hold equal YES on platform A and NO on platform B
        // Regardless of outcome, we get $1 per contract pair
        let balanced_contracts = self.matched_contracts();
        balanced_contracts - self.total_cost()
    }
    
    /// Number of matched contract pairs (min of YES and NO across platforms)
    pub fn matched_contracts(&self) -> f64 {
        let yes_total = self.kalshi_yes.contracts + self.poly_yes.contracts;
        let no_total = self.kalshi_no.contracts + self.poly_no.contracts;
        yes_total.min(no_total)
    }
    
    /// Unmatched exposure (contracts without offsetting position)
    pub fn unmatched_exposure(&self) -> f64 {
        let yes_total = self.kalshi_yes.contracts + self.poly_yes.contracts;
        let no_total = self.kalshi_no.contracts + self.poly_no.contracts;
        (yes_total - no_total).abs()
    }
    
    /// Mark position as resolved with outcome
    pub fn resolve(&mut self, outcome_yes_won: bool) {
        let payout = if outcome_yes_won {
            // YES won: Kalshi YES + Poly YES pay out
            self.kalshi_yes.contracts + self.poly_yes.contracts
        } else {
            // NO won: Kalshi NO + Poly NO pay out
            self.kalshi_no.contracts + self.poly_no.contracts
        };
        
        self.realized_pnl = Some(payout - self.total_cost());
        self.status = "resolved".to_string();
    }
}

/// Summary of all positions
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct PositionSummary {
    /// Total cost basis across all open positions
    pub total_cost_basis: f64,
    
    /// Total guaranteed profit from matched arbs
    pub total_guaranteed_profit: f64,
    
    /// Total unmatched exposure (risk)
    pub total_unmatched_exposure: f64,
    
    /// Total realized P&L from closed/resolved positions
    pub realized_pnl: f64,
    
    /// Number of open positions
    pub open_positions: usize,
    
    /// Number of resolved positions
    pub resolved_positions: usize,
    
    /// Total contracts held
    pub total_contracts: f64,
}

/// Position tracker with persistence
#[derive(Debug, Serialize, Deserialize)]
pub struct PositionTracker {
    /// All positions keyed by market_id
    positions: HashMap<String, ArbPosition>,
}

/// Data structure for serialization
#[derive(Serialize)]
struct SaveData {
    positions: HashMap<String, ArbPosition>,
}

impl Default for PositionTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl PositionTracker {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
        }
    }
    
    /// Load from file or create new
    pub fn load() -> Self {
        Self::load_from(crate::paths::resolve_workspace_file(POSITION_FILE))
    }
    
    pub fn load_from<P: AsRef<Path>>(path: P) -> Self {
        match std::fs::read_to_string(path.as_ref()) {
            Ok(contents) => {
                match serde_json::from_str::<Self>(&contents) {
                    Ok(tracker) => {
                        info!("[POSITIONS] Loaded {} positions from {:?}",
                              tracker.positions.len(), path.as_ref());
                        tracker
                    }
                    Err(e) => {
                        warn!("[POSITIONS] Failed to parse positions file: {}", e);
                        Self::new()
                    }
                }
            }
            Err(_) => {
                info!("[POSITIONS] No positions file found, starting fresh");
                Self::new()
            }
        }
    }
    
    /// Save to file
    pub fn save(&self) -> Result<()> {
        self.save_to(crate::paths::resolve_workspace_file(POSITION_FILE))
    }
    
    pub fn save_to<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
    
    /// Save positions
    pub fn save_async(&self) {
        // Clone data for serialization
        let data = SaveData {
            positions: self.positions.clone(),
        };
        // Try to spawn on runtime; if no runtime, save synchronously
        let path = crate::paths::resolve_workspace_file(POSITION_FILE);
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                if let Ok(json) = serde_json::to_string_pretty(&data) {
                    if let Err(e) = tokio::fs::write(&path, json).await {
                        tracing::error!("[POSITION] Failed to save positions to {:?}: {}", path, e);
                    }
                }
            });
        } else if let Ok(json) = serde_json::to_string_pretty(&data) {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::error!("[POSITION] Failed to save positions to {:?}: {}", path, e);
            }
        }
    }
    
    /// Record a fill
    pub fn record_fill(&mut self, fill: &FillRecord) {
        self.record_fill_internal(fill);
        self.save_async();
    }

    /// Record a fill without saving
    pub fn record_fill_internal(&mut self, fill: &FillRecord) {
        let position = self.positions
            .entry(fill.market_id.clone())
            .or_insert_with(|| ArbPosition::new(&fill.market_id, &fill.description));

        // Update the position leg (only if actually filled)
        if fill.contracts.abs() > 0.0 {
            match (fill.platform.as_str(), fill.side.as_str()) {
                ("kalshi", "yes") => position.kalshi_yes.add(fill.contracts, fill.price),
                ("kalshi", "no") => position.kalshi_no.add(fill.contracts, fill.price),
                ("polymarket", "yes") => position.poly_yes.add(fill.contracts, fill.price),
                ("polymarket", "no") => position.poly_no.add(fill.contracts, fill.price),
                _ => warn!("[POSITIONS] Unknown platform/side: {}/{}", fill.platform, fill.side),
            }
        }

        position.total_fees += fill.fees;

        // Record in trade history
        position.record_trade(
            fill.reason.clone(),
            &fill.platform,
            &fill.side,
            fill.requested_contracts,
            fill.contracts,
            fill.price,
            fill.status.clone(),
            fill.failure_reason.clone(),
            if fill.order_id.is_empty() { None } else { Some(fill.order_id.clone()) },
        );

        info!("[POSITIONS] Recorded fill: {} {} {} @{:.1}¢ x{:.0} (fees: ${:.4}) [reason={:?}, status={:?}]",
              fill.platform, fill.side, fill.market_id,
              fill.price * 100.0, fill.contracts, fill.fees,
              fill.reason, fill.status);
    }
    
    /// Get or create position for a market
    pub fn get_or_create(&mut self, market_id: &str, description: &str) -> &mut ArbPosition {
        self.positions
            .entry(market_id.to_string())
            .or_insert_with(|| ArbPosition::new(market_id, description))
    }
    
    /// Get position (if exists)
    pub fn get(&self, market_id: &str) -> Option<&ArbPosition> {
        self.positions.get(market_id)
    }
    
    /// Mark a position as resolved
    pub fn resolve_position(&mut self, market_id: &str, yes_won: bool) -> Option<f64> {
        if let Some(position) = self.positions.get_mut(market_id) {
            position.resolve(yes_won);
            let pnl = position.realized_pnl.unwrap_or(0.0);

            info!("[POSITIONS] Resolved {}: {} won, P&L: ${:.2}",
                  market_id, if yes_won { "YES" } else { "NO" }, pnl);

            self.save_async();
            Some(pnl)
        } else {
            None
        }
    }
    
    /// Get summary statistics
    pub fn summary(&self) -> PositionSummary {
        let mut summary = PositionSummary::default();
        
        for position in self.positions.values() {
            match position.status.as_str() {
                "open" => {
                    summary.open_positions += 1;
                    summary.total_cost_basis += position.total_cost();
                    summary.total_guaranteed_profit += position.guaranteed_profit();
                    summary.total_unmatched_exposure += position.unmatched_exposure();
                    summary.total_contracts += position.total_contracts();
                }
                "resolved" => {
                    summary.resolved_positions += 1;
                    summary.realized_pnl += position.realized_pnl.unwrap_or(0.0);
                }
                _ => {}
            }
        }
        
        summary
    }
    
    /// Get all open positions
    pub fn open_positions(&self) -> Vec<&ArbPosition> {
        self.positions.values()
            .filter(|p| p.status == "open")
            .collect()
    }
    
}

/// Record of a single fill (used for position updates and trade history)
#[derive(Debug, Clone)]
pub struct FillRecord {
    pub market_id: String,
    pub description: String,
    pub platform: String,   // "kalshi" or "polymarket"
    pub side: String,       // "yes" or "no"
    pub contracts: f64,     // filled contracts (negative for sells/closes)
    pub price: f64,
    pub fees: f64,
    pub order_id: String,
    #[allow(dead_code)]
    pub timestamp: String,
    /// Why this trade was attempted
    pub reason: TradeReason,
    /// Number of contracts originally requested (may differ from filled)
    pub requested_contracts: f64,
    /// Status of the trade
    pub status: TradeStatus,
    /// If failed/blocked, the reason why
    pub failure_reason: Option<String>,
}

impl FillRecord {
    /// Create a new FillRecord for a successful fill (legacy API for backward compatibility)
    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)]
    pub fn new(
        market_id: &str,
        description: &str,
        platform: &str,
        side: &str,
        contracts: f64,
        price: f64,
        fees: f64,
        order_id: &str,
    ) -> Self {
        // Determine reason based on contracts sign and context
        // Negative contracts = close/sell, positive = buy
        let reason = if contracts < 0.0 {
            TradeReason::AutoClose
        } else if side == "yes" {
            TradeReason::ArbLegYes
        } else {
            TradeReason::ArbLegNo
        };

        Self {
            market_id: market_id.to_string(),
            description: description.to_string(),
            platform: platform.to_string(),
            side: side.to_string(),
            contracts,
            price,
            fees,
            order_id: order_id.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            reason,
            requested_contracts: contracts, // For legacy API, assume requested = filled
            status: if contracts.abs() > 0.0 { TradeStatus::Success } else { TradeStatus::Failed },
            failure_reason: None,
        }
    }

    /// Create a FillRecord with full trade history details
    #[allow(clippy::too_many_arguments)]
    pub fn with_details(
        market_id: &str,
        description: &str,
        platform: &str,
        side: &str,
        requested_contracts: f64,
        filled_contracts: f64,
        price: f64,
        fees: f64,
        order_id: &str,
        reason: TradeReason,
        status: TradeStatus,
        failure_reason: Option<String>,
    ) -> Self {
        Self {
            market_id: market_id.to_string(),
            description: description.to_string(),
            platform: platform.to_string(),
            side: side.to_string(),
            contracts: filled_contracts,
            price,
            fees,
            order_id: order_id.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            reason,
            requested_contracts,
            status,
            failure_reason,
        }
    }
}

#[allow(dead_code)]
pub type SharedPositionTracker = Arc<RwLock<PositionTracker>>;

#[allow(dead_code)]
pub fn create_position_tracker() -> SharedPositionTracker {
    Arc::new(RwLock::new(PositionTracker::load()))
}

#[derive(Clone)]
pub struct PositionChannel {
    tx: mpsc::UnboundedSender<FillRecord>,
}

impl PositionChannel {
    pub fn new(tx: mpsc::UnboundedSender<FillRecord>) -> Self {
        Self { tx }
    }

    #[inline]
    pub fn record_fill(&self, fill: FillRecord) {
        if let Err(e) = self.tx.send(fill) {
            // This is critical - losing fills means incorrect P&L and audit trail
            tracing::error!(
                "[POSITION] Failed to record fill - DATA LOSS! Fill: {:?}, Error: {}",
                e.0, e
            );
        }
    }
}

pub fn create_position_channel() -> (PositionChannel, mpsc::UnboundedReceiver<FillRecord>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (PositionChannel::new(tx), rx)
}

pub async fn position_writer_loop(
    mut rx: mpsc::UnboundedReceiver<FillRecord>,
    tracker: Arc<RwLock<PositionTracker>>,
) {
    let mut batch = Vec::with_capacity(16);
    let mut interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            biased;

            Some(fill) = rx.recv() => {
                batch.push(fill);
                if batch.len() >= 16 {
                    let mut guard = tracker.write().await;
                    for fill in batch.drain(..) {
                        guard.record_fill_internal(&fill);
                    }
                    guard.save_async();
                }
            }
            _ = interval.tick() => {
                if !batch.is_empty() {
                    let mut guard = tracker.write().await;
                    for fill in batch.drain(..) {
                        guard.record_fill_internal(&fill);
                    }
                    guard.save_async();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_position_leg() {
        let mut leg = PositionLeg::default();
        leg.add(10.0, 0.45); // Buy 10 contracts at 45¢
        
        assert_eq!(leg.contracts, 10.0);
        assert!((leg.cost_basis - 4.50).abs() < 0.001);
        assert!((leg.avg_price - 0.45).abs() < 0.001);
        
        // Profit if this leg wins
        assert!((leg.profit_if_win() - 5.50).abs() < 0.001); // $10 payout - $4.50 cost
    }
    
    #[test]
    fn test_arb_position_guaranteed_profit() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        // Buy 10 YES on Poly at 45¢
        pos.poly_yes.add(10.0, 0.45);
        
        // Buy 10 NO on Kalshi at 50¢
        pos.kalshi_no.add(10.0, 0.50);
        
        // Total cost: $4.50 + $5.00 = $9.50
        // Guaranteed payout: $10.00 (one side wins)
        // Guaranteed profit: $0.50
        
        assert!((pos.total_cost() - 9.50).abs() < 0.001);
        assert!((pos.matched_contracts() - 10.0).abs() < 0.001);
        assert!((pos.guaranteed_profit() - 0.50).abs() < 0.001);
        assert!((pos.unmatched_exposure() - 0.0).abs() < 0.001);
    }
    
    #[test]
    fn test_unmatched_exposure() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        // Buy 10 YES on Poly
        pos.poly_yes.add(10.0, 0.45);
        
        // Buy only 8 NO on Kalshi (partial fill)
        pos.kalshi_no.add(8.0, 0.50);
        
        // Matched: 8, Unmatched: 2
        assert!((pos.matched_contracts() - 8.0).abs() < 0.001);
        assert!((pos.unmatched_exposure() - 2.0).abs() < 0.001);
    }
    
    #[test]
    fn test_resolution() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        pos.poly_yes.add(10.0, 0.45);
        pos.kalshi_no.add(10.0, 0.50);

        // YES wins
        pos.resolve(true);

        // Payout: 10 (poly_yes wins)
        // Cost: 9.50
        // P&L: +0.50
        assert!((pos.realized_pnl.unwrap() - 0.50).abs() < 0.001);
        assert_eq!(pos.status, "resolved");
    }

    // =========================================================================
    // TRADE HISTORY TESTS
    // =========================================================================

    #[test]
    fn test_trade_history_records_success() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");

        let seq = pos.record_trade(
            TradeReason::ArbLegYes,
            "polymarket",
            "yes",
            10.0,  // requested
            10.0,  // filled
            0.45,  // price
            TradeStatus::Success,
            None,
            Some("order-123".to_string()),
        );

        assert_eq!(seq, 0, "First trade should have sequence 0");
        assert_eq!(pos.trades.len(), 1);

        let trade = &pos.trades[0];
        assert_eq!(trade.sequence, 0);
        assert_eq!(trade.reason, TradeReason::ArbLegYes);
        assert_eq!(trade.platform, "polymarket");
        assert_eq!(trade.side, "yes");
        assert_eq!(trade.requested_contracts, 10.0);
        assert_eq!(trade.filled_contracts, 10.0);
        assert!((trade.price - 0.45).abs() < 0.001);
        assert_eq!(trade.status, TradeStatus::Success);
        assert!(trade.failure_reason.is_none());
        assert_eq!(trade.order_id, Some("order-123".to_string()));
    }

    #[test]
    fn test_trade_history_records_failure() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");

        pos.record_trade(
            TradeReason::ArbLegNo,
            "kalshi",
            "no",
            10.0,  // requested
            0.0,   // filled (failed)
            0.50,  // price
            TradeStatus::Failed,
            Some("Insufficient balance".to_string()),
            None,
        );

        assert_eq!(pos.trades.len(), 1);

        let trade = &pos.trades[0];
        assert_eq!(trade.reason, TradeReason::ArbLegNo);
        assert_eq!(trade.status, TradeStatus::Failed);
        assert_eq!(trade.filled_contracts, 0.0);
        assert_eq!(trade.failure_reason, Some("Insufficient balance".to_string()));
        assert!(trade.order_id.is_none());
    }

    #[test]
    fn test_trade_history_records_partial_fill() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");

        pos.record_trade(
            TradeReason::ArbLegYes,
            "polymarket",
            "yes",
            10.0,  // requested
            7.0,   // filled (partial)
            0.45,
            TradeStatus::PartialFill,
            None,
            Some("order-456".to_string()),
        );

        let trade = &pos.trades[0];
        assert_eq!(trade.status, TradeStatus::PartialFill);
        assert_eq!(trade.requested_contracts, 10.0);
        assert_eq!(trade.filled_contracts, 7.0);
    }

    #[test]
    fn test_trade_history_records_auto_close_retries() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");

        // First close attempt (partial)
        pos.record_trade(
            TradeReason::AutoClose,
            "kalshi",
            "no",
            -10.0,  // negative = closing
            -7.0,   // partial fill
            0.18,
            TradeStatus::PartialFill,
            None,
            Some("order-100".to_string()),
        );

        // Retry at lower price
        pos.record_trade(
            TradeReason::AutoCloseRetry,
            "kalshi",
            "no",
            -3.0,   // remaining
            -3.0,   // filled
            0.17,
            TradeStatus::Success,
            None,
            Some("order-101".to_string()),
        );

        assert_eq!(pos.trades.len(), 2);

        let first = &pos.trades[0];
        assert_eq!(first.reason, TradeReason::AutoClose);
        assert_eq!(first.sequence, 0);

        let retry = &pos.trades[1];
        assert_eq!(retry.reason, TradeReason::AutoCloseRetry);
        assert_eq!(retry.sequence, 1);
        assert!((retry.price - 0.17).abs() < 0.001);
    }

    #[test]
    fn test_trade_history_records_circuit_breaker_block() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");

        pos.record_trade(
            TradeReason::CircuitBreakerBlock,
            "kalshi",
            "no",
            50.0,  // requested
            0.0,   // blocked, nothing filled
            0.19,
            TradeStatus::Blocked,
            Some("max_position_per_market exceeded (limit: 10)".to_string()),
            None,
        );

        let trade = &pos.trades[0];
        assert_eq!(trade.reason, TradeReason::CircuitBreakerBlock);
        assert_eq!(trade.status, TradeStatus::Blocked);
        assert_eq!(trade.filled_contracts, 0.0);
        assert!(trade.failure_reason.is_some());
    }

    #[test]
    fn test_trade_history_sequence_incrementing() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");

        for i in 0..5 {
            let seq = pos.record_trade(
                TradeReason::ArbLegYes,
                "polymarket",
                "yes",
                1.0, 1.0, 0.50,
                TradeStatus::Success,
                None, None,
            );
            assert_eq!(seq, i as u32, "Sequence should increment");
        }

        assert_eq!(pos.trades.len(), 5);
        for (i, trade) in pos.trades.iter().enumerate() {
            assert_eq!(trade.sequence, i as u32);
        }
    }

    #[test]
    fn test_trade_history_persists_via_fill_record() {
        let mut tracker = PositionTracker::new();

        // Record a fill with full details
        let fill = FillRecord::with_details(
            "TEST-MARKET",
            "Test Market",
            "polymarket",
            "yes",
            10.0,  // requested
            10.0,  // filled
            0.45,
            0.0,   // fees
            "order-123",
            TradeReason::ArbLegYes,
            TradeStatus::Success,
            None,
        );
        tracker.record_fill_internal(&fill);

        let pos = tracker.get("TEST-MARKET").expect("Position should exist");
        assert_eq!(pos.trades.len(), 1);

        let trade = &pos.trades[0];
        assert_eq!(trade.reason, TradeReason::ArbLegYes);
        assert_eq!(trade.status, TradeStatus::Success);
        assert_eq!(trade.filled_contracts, 10.0);
    }

    #[test]
    fn test_trade_history_records_zero_fills() {
        let mut tracker = PositionTracker::new();

        // Record a failed fill (0 contracts)
        let fill = FillRecord::with_details(
            "TEST-MARKET",
            "Test Market",
            "kalshi",
            "no",
            10.0,  // requested
            0.0,   // filled (failed)
            0.50,
            0.0,
            "",
            TradeReason::ArbLegNo,
            TradeStatus::Failed,
            Some("No fill".to_string()),
        );
        tracker.record_fill_internal(&fill);

        let pos = tracker.get("TEST-MARKET").expect("Position should exist");

        // Trade history should have the record even though no contracts filled
        assert_eq!(pos.trades.len(), 1);
        assert_eq!(pos.trades[0].status, TradeStatus::Failed);
        assert_eq!(pos.trades[0].filled_contracts, 0.0);

        // Position leg should NOT be updated for zero-fill
        assert_eq!(pos.kalshi_no.contracts, 0.0);
    }

    #[test]
    fn test_trade_history_complete_arb_scenario() {
        let mut tracker = PositionTracker::new();

        // Scenario: Arb attempt where YES fills but NO fails, then auto-close
        // 1. Poly YES fills 10
        let yes_fill = FillRecord::with_details(
            "TEST-MARKET", "Test", "polymarket", "yes",
            10.0, 10.0, 0.45, 0.0, "order-1",
            TradeReason::ArbLegYes, TradeStatus::Success, None,
        );
        tracker.record_fill_internal(&yes_fill);

        // 2. Kalshi NO fails
        let no_fill = FillRecord::with_details(
            "TEST-MARKET", "Test", "kalshi", "no",
            10.0, 0.0, 0.50, 0.0, "",
            TradeReason::ArbLegNo, TradeStatus::Failed, Some("No liquidity".to_string()),
        );
        tracker.record_fill_internal(&no_fill);

        // 3. Auto-close attempt 1 (partial)
        let close1 = FillRecord::with_details(
            "TEST-MARKET", "Test", "polymarket", "yes",
            -10.0, -7.0, 0.44, 0.0, "order-2",
            TradeReason::AutoClose, TradeStatus::PartialFill, None,
        );
        tracker.record_fill_internal(&close1);

        // 4. Auto-close retry (complete)
        let close2 = FillRecord::with_details(
            "TEST-MARKET", "Test", "polymarket", "yes",
            -3.0, -3.0, 0.43, 0.0, "order-3",
            TradeReason::AutoCloseRetry, TradeStatus::Success, None,
        );
        tracker.record_fill_internal(&close2);

        let pos = tracker.get("TEST-MARKET").expect("Position should exist");

        // Should have 4 trades in history
        assert_eq!(pos.trades.len(), 4);

        // Verify sequence order
        assert_eq!(pos.trades[0].sequence, 0);
        assert_eq!(pos.trades[1].sequence, 1);
        assert_eq!(pos.trades[2].sequence, 2);
        assert_eq!(pos.trades[3].sequence, 3);

        // Verify reasons
        assert_eq!(pos.trades[0].reason, TradeReason::ArbLegYes);
        assert_eq!(pos.trades[1].reason, TradeReason::ArbLegNo);
        assert_eq!(pos.trades[2].reason, TradeReason::AutoClose);
        assert_eq!(pos.trades[3].reason, TradeReason::AutoCloseRetry);

        // Net position should be 0 after auto-close
        // +10 - 7 - 3 = 0
        let net = pos.poly_yes.contracts;
        assert!(net.abs() < 0.001, "Net position should be 0, got {}", net);
    }
}