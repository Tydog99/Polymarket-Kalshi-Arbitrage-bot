//! Shared types for order execution.

/// Trading platform identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Kalshi,
    Polymarket,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Kalshi => write!(f, "Kalshi"),
            Platform::Polymarket => write!(f, "Polymarket"),
        }
    }
}

/// Order action (buy or sell)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderAction {
    Buy,
    Sell,
}

impl std::fmt::Display for OrderAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderAction::Buy => write!(f, "BUY"),
            OrderAction::Sell => write!(f, "SELL"),
        }
    }
}

/// Outcome side (yes or no)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeSide {
    Yes,
    No,
}

impl OutcomeSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutcomeSide::Yes => "yes",
            OutcomeSide::No => "no",
        }
    }
}

impl std::fmt::Display for OutcomeSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Request to execute a single leg of an arbitrage
pub struct LegRequest<'a> {
    pub leg_id: &'a str,
    pub platform: Platform,
    pub action: OrderAction,
    pub side: OutcomeSide,
    pub price_cents: u16,
    pub contracts: i64,
    pub kalshi_ticker: Option<&'a str>,
    pub poly_token: Option<&'a str>,
}

/// Result of leg execution
#[derive(Debug)]
pub struct LegResult {
    pub success: bool,
    pub latency_ns: u64,
    pub error: Option<String>,
}

impl LegResult {
    pub fn ok(latency_ns: u64) -> Self {
        Self { success: true, latency_ns, error: None }
    }

    pub fn err(latency_ns: u64, error: impl Into<String>) -> Self {
        Self { success: false, latency_ns, error: Some(error.into()) }
    }
}

