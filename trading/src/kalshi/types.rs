//! Kalshi order types.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Debug, Clone, Serialize)]
pub struct KalshiOrderRequest<'a> {
    pub ticker: Cow<'a, str>,
    pub action: &'static str,
    pub side: &'static str,
    #[serde(rename = "type")]
    pub order_type: &'static str,
    pub count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_price: Option<i64>,
    pub client_order_id: Cow<'a, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<&'static str>,
}

impl<'a> KalshiOrderRequest<'a> {
    /// Create an IOC (immediate-or-cancel) buy order
    pub fn ioc_buy(ticker: Cow<'a, str>, side: &'static str, price_cents: i64, count: i64, client_order_id: Cow<'a, str>) -> Self {
        let (yes_price, no_price) = if side == "yes" {
            (Some(price_cents), None)
        } else {
            (None, Some(price_cents))
        };

        Self {
            ticker,
            action: "buy",
            side,
            order_type: "limit",
            count,
            yes_price,
            no_price,
            client_order_id,
            expiration_ts: None,
            time_in_force: Some("immediate_or_cancel"),
        }
    }

    /// Create an IOC (immediate-or-cancel) sell order
    pub fn ioc_sell(ticker: Cow<'a, str>, side: &'static str, price_cents: i64, count: i64, client_order_id: Cow<'a, str>) -> Self {
        let (yes_price, no_price) = if side == "yes" {
            (Some(price_cents), None)
        } else {
            (None, Some(price_cents))
        };

        Self {
            ticker,
            action: "sell",
            side,
            order_type: "limit",
            count,
            yes_price,
            no_price,
            client_order_id,
            expiration_ts: None,
            time_in_force: Some("immediate_or_cancel"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrderResponse {
    pub order: KalshiOrderDetails,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrderDetails {
    pub order_id: String,
    pub ticker: String,
    pub status: String,        // "resting", "canceled", "executed", "pending"
    #[serde(default)]
    pub remaining_count: Option<i64>,
    #[serde(default)]
    pub queue_position: Option<i64>,
    pub action: String,
    pub side: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub yes_price: Option<i64>,
    pub no_price: Option<i64>,
    pub created_time: Option<String>,
    #[serde(default)]
    pub taker_fill_count: Option<i64>,
    #[serde(default)]
    pub maker_fill_count: Option<i64>,
    #[serde(default)]
    pub place_count: Option<i64>,
    #[serde(default)]
    pub taker_fill_cost: Option<i64>,
    #[serde(default)]
    pub maker_fill_cost: Option<i64>,
}

#[allow(dead_code)]
impl KalshiOrderDetails {
    /// Total filled contracts
    pub fn filled_count(&self) -> i64 {
        self.taker_fill_count.unwrap_or(0) + self.maker_fill_count.unwrap_or(0)
    }

    /// Check if order was fully filled
    pub fn is_filled(&self) -> bool {
        self.status == "executed" || self.remaining_count == Some(0)
    }

    /// Check if order was partially filled
    pub fn is_partial(&self) -> bool {
        self.filled_count() > 0 && !self.is_filled()
    }
}

// ============================================================================
// Portfolio Positions Types
// ============================================================================

/// Response from GET /portfolio/positions
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiPositionsResponse {
    pub market_positions: Vec<KalshiMarketPosition>,
    #[serde(default)]
    pub event_positions: Vec<KalshiEventPosition>,
    pub cursor: Option<String>,
}

/// A single market position
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiMarketPosition {
    pub ticker: String,
    /// Net position: positive = long YES, negative = long NO
    pub position: i64,
    /// Total contracts traded in this market
    pub total_traded: i64,
    /// Current market exposure in cents
    pub market_exposure: i64,
    /// Realized P&L in cents
    pub realized_pnl: i64,
    /// Fees paid in cents
    pub fees_paid: i64,
    /// Number of resting orders
    #[serde(default)]
    pub resting_orders_count: i64,
}

/// A single event position (aggregate across markets in an event)
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiEventPosition {
    pub event_ticker: String,
    pub total_cost: i64,
    pub event_exposure: i64,
    pub realized_pnl: i64,
    pub fees_paid: i64,
}
