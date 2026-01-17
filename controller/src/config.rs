//! System configuration and league mapping definitions.
//!
//! This module contains all configuration constants, league mappings, and
//! environment variable parsing for the trading system.

/// Kalshi WebSocket URL
pub const KALSHI_WS_URL: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";

/// Kalshi REST API base URL
pub const KALSHI_API_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";

/// Polymarket WebSocket URL
pub const POLYMARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Gamma API base URL (Polymarket market data)
pub const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

/// Kalshi web market URL base (for user-facing links)
pub const KALSHI_WEB_BASE: &str = "https://kalshi.com/markets";

/// Polymarket web base URL (for user-facing links)
pub const POLYMARKET_WEB_BASE: &str = "https://polymarket.com";

/// Build a user-facing Polymarket URL from a market slug and league
/// Uses /event/{base_slug} format (strips market-type suffixes like -total-3pt5, -ata, etc.)
pub fn build_polymarket_url(_league: &str, poly_slug: &str) -> String {
    // Extract base game slug (strip market-type suffixes)
    let base_slug = extract_base_game_slug(poly_slug);
    format!("{}/event/{}", POLYMARKET_WEB_BASE, base_slug)
}

/// Extract base game slug by removing market-type suffixes
/// "sea-pis-ata-2026-01-16-ata" → "sea-pis-ata-2026-01-16"
/// "sea-pis-ata-2026-01-16-total-3pt5" → "sea-pis-ata-2026-01-16"
fn extract_base_game_slug(slug: &str) -> &str {
    // Look for YYYY-MM-DD pattern and truncate after it
    // Slug format: {league}-{team1}-{team2}-YYYY-MM-DD[-suffix]
    let bytes = slug.as_bytes();
    let len = bytes.len();

    // Need at least 11 chars for the date part (-YYYY-MM-DD)
    if len < 11 {
        return slug;
    }

    // Scan for date pattern: -YYYY-MM-DD
    for i in 0..len.saturating_sub(10) {
        if bytes[i] == b'-'
            && i + 11 <= len
            && bytes[i+1..i+5].iter().all(|&b| b.is_ascii_digit())
            && bytes[i+5] == b'-'
            && bytes[i+6..i+8].iter().all(|&b| b.is_ascii_digit())
            && bytes[i+8] == b'-'
            && bytes[i+9..i+11].iter().all(|&b| b.is_ascii_digit())
        {
            // Found date ending at i+11
            return &slug[..i + 11];
        }
    }
    slug
}

/// Arb threshold: alert when total cost < this (e.g., 0.995 = 0.5% profit)
pub const ARB_THRESHOLD: f64 = 0.995;

/// Polymarket ping interval (seconds) - keep connection alive
pub const POLY_PING_INTERVAL_SECS: u64 = 30;

/// Kalshi API rate limit delay (milliseconds between requests)
/// Kalshi limit: 20 req/sec = 50ms minimum. We use 60ms for safety margin.
pub const KALSHI_API_DELAY_MS: u64 = 60;

/// WebSocket reconnect delay (seconds)
pub const WS_RECONNECT_DELAY_SECS: u64 = 5;

/// Which leagues to monitor (empty = all)
/// Set ENABLED_LEAGUES env var to comma-separated list, e.g., "cs2,lol,cod"
pub fn enabled_leagues() -> &'static [String] {
    static CACHED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CACHED.get_or_init(|| {
        std::env::var("ENABLED_LEAGUES")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|l| l.trim().to_lowercase()).collect())
            .unwrap_or_default()
    })
}

/// Which leagues to discover but not trade (monitor only)
/// Set DISABLED_LEAGUES env var to comma-separated list, e.g., "epl,seriea"
pub fn disabled_leagues() -> &'static HashSet<String> {
    static CACHED: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    CACHED.get_or_init(|| {
        std::env::var("DISABLED_LEAGUES")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|l| l.trim().to_lowercase()).collect())
            .unwrap_or_default()
    })
}

/// Check if a league is disabled for trading (monitor only)
pub fn is_league_disabled(league: &str) -> bool {
    disabled_leagues().contains(&league.to_lowercase())
}

/// Enable verbose heartbeat output with hierarchical tree view.
/// Set `VERBOSE_HEARTBEAT=1` or use `--verbose-heartbeat` CLI flag.
pub fn verbose_heartbeat_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("VERBOSE_HEARTBEAT")
            .map(|v| v == "1" || v.to_lowercase() == "true" || v.to_lowercase() == "yes")
            .unwrap_or(false)
    })
}

/// Heartbeat interval in seconds for arbitrage detection loop.
/// Set `HEARTBEAT_INTERVAL_SECS=N` or use `--heartbeat-interval=N` CLI flag (default: 10 seconds).
pub fn heartbeat_interval_secs() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("HEARTBEAT_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10)
    })
}

/// Price logging enabled (set PRICE_LOGGING=1 to enable)
#[allow(dead_code)]
pub fn price_logging_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("PRICE_LOGGING")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false)
    })
}

/// League configuration for market discovery
#[derive(Debug, Clone)]
pub struct LeagueConfig {
    pub league_code: &'static str,
    pub poly_prefix: &'static str,
    pub kalshi_series_game: &'static str,
    pub kalshi_series_spread: Option<&'static str>,
    pub kalshi_series_total: Option<&'static str>,
    pub kalshi_series_btts: Option<&'static str>,
    /// Polymarket series ID for event-based discovery (esports).
    /// None for traditional sports that use slug-based matching.
    pub poly_series_id: Option<&'static str>,
    /// Kalshi web URL slug for this league (e.g., "counterstrike-2-game")
    pub kalshi_web_slug: &'static str,
}

/// Get all supported leagues with their configurations
pub fn get_league_configs() -> Vec<LeagueConfig> {
    vec![
        // Major European leagues (full market types)
        LeagueConfig {
            league_code: "epl",
            poly_prefix: "epl",
            kalshi_series_game: "KXEPLGAME",
            kalshi_series_spread: Some("KXEPLSPREAD"),
            kalshi_series_total: Some("KXEPLTOTAL"),
            kalshi_series_btts: Some("KXEPLBTTS"),
            poly_series_id: None,
            kalshi_web_slug: "premier-league-game",
        },
        LeagueConfig {
            league_code: "bundesliga",
            poly_prefix: "bun",
            kalshi_series_game: "KXBUNDESLIGAGAME",
            kalshi_series_spread: Some("KXBUNDESLIGASPREAD"),
            kalshi_series_total: Some("KXBUNDESLIGATOTAL"),
            kalshi_series_btts: Some("KXBUNDESLIGABTTS"),
            poly_series_id: None,
            kalshi_web_slug: "bundesliga-game",
        },
        LeagueConfig {
            league_code: "laliga",
            poly_prefix: "lal",
            kalshi_series_game: "KXLALIGAGAME",
            kalshi_series_spread: Some("KXLALIGASPREAD"),
            kalshi_series_total: Some("KXLALIGATOTAL"),
            kalshi_series_btts: Some("KXLALIGABTTS"),
            poly_series_id: None,
            kalshi_web_slug: "la-liga-game",
        },
        LeagueConfig {
            league_code: "seriea",
            poly_prefix: "sea",
            kalshi_series_game: "KXSERIEAGAME",
            kalshi_series_spread: Some("KXSERIEASPREAD"),
            kalshi_series_total: Some("KXSERIEATOTAL"),
            kalshi_series_btts: Some("KXSERIEABTTS"),
            poly_series_id: None,
            kalshi_web_slug: "serie-a-game",
        },
        LeagueConfig {
            league_code: "ligue1",
            poly_prefix: "fl1",
            kalshi_series_game: "KXLIGUE1GAME",
            kalshi_series_spread: Some("KXLIGUE1SPREAD"),
            kalshi_series_total: Some("KXLIGUE1TOTAL"),
            kalshi_series_btts: Some("KXLIGUE1BTTS"),
            poly_series_id: None,
            kalshi_web_slug: "ligue-1-game",
        },
        LeagueConfig {
            league_code: "ucl",
            poly_prefix: "ucl",
            kalshi_series_game: "KXUCLGAME",
            kalshi_series_spread: Some("KXUCLSPREAD"),
            kalshi_series_total: Some("KXUCLTOTAL"),
            kalshi_series_btts: Some("KXUCLBTTS"),
            poly_series_id: None,
            kalshi_web_slug: "champions-league-game",
        },
        // Secondary European leagues (moneyline only)
        LeagueConfig {
            league_code: "uel",
            poly_prefix: "uel",
            kalshi_series_game: "KXUELGAME",
            kalshi_series_spread: None,
            kalshi_series_total: None,
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "europa-league-game",
        },
        LeagueConfig {
            league_code: "eflc",
            poly_prefix: "elc",
            kalshi_series_game: "KXEFLCHAMPIONSHIPGAME",
            kalshi_series_spread: None,
            kalshi_series_total: None,
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "efl-championship-game",
        },
        // US Sports
        LeagueConfig {
            league_code: "nba",
            poly_prefix: "nba",
            kalshi_series_game: "KXNBAGAME",
            kalshi_series_spread: Some("KXNBASPREAD"),
            kalshi_series_total: Some("KXNBATOTAL"),
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "nba-game",
        },
        LeagueConfig {
            league_code: "nfl",
            poly_prefix: "nfl",
            kalshi_series_game: "KXNFLGAME",
            kalshi_series_spread: Some("KXNFLSPREAD"),
            kalshi_series_total: Some("KXNFLTOTAL"),
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "nfl-game",
        },
        LeagueConfig {
            league_code: "nhl",
            poly_prefix: "nhl",
            kalshi_series_game: "KXNHLGAME",
            kalshi_series_spread: Some("KXNHLSPREAD"),
            kalshi_series_total: Some("KXNHLTOTAL"),
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "nhl-game",
        },
        LeagueConfig {
            league_code: "mlb",
            poly_prefix: "mlb",
            kalshi_series_game: "KXMLBGAME",
            kalshi_series_spread: Some("KXMLBSPREAD"),
            kalshi_series_total: Some("KXMLBTOTAL"),
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "mlb-game",
        },
        LeagueConfig {
            league_code: "mls",
            poly_prefix: "mls",
            kalshi_series_game: "KXMLSGAME",
            kalshi_series_spread: None,
            kalshi_series_total: None,
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "mls-game",
        },
        LeagueConfig {
            league_code: "ncaaf",
            poly_prefix: "cfb",
            kalshi_series_game: "KXNCAAFGAME",
            kalshi_series_spread: Some("KXNCAAFSPREAD"),
            kalshi_series_total: Some("KXNCAAFTOTAL"),
            kalshi_series_btts: None,
            poly_series_id: None,
            kalshi_web_slug: "ncaaf-game",
        },
        // Esports
        LeagueConfig {
            league_code: "cs2",
            poly_prefix: "cs2",
            kalshi_series_game: "KXCS2GAME",
            kalshi_series_spread: None,
            kalshi_series_total: None,
            kalshi_series_btts: None,
            poly_series_id: Some("10310"),
            kalshi_web_slug: "counterstrike-2-game",
        },
        LeagueConfig {
            league_code: "lol",
            poly_prefix: "lol",
            kalshi_series_game: "KXLOLGAME",
            kalshi_series_spread: None,
            kalshi_series_total: None,
            kalshi_series_btts: None,
            poly_series_id: Some("10311"),
            kalshi_web_slug: "league-of-legends-game",
        },
        LeagueConfig {
            league_code: "cod",
            poly_prefix: "codmw",
            kalshi_series_game: "KXCODGAME",
            kalshi_series_spread: None,
            kalshi_series_total: None,
            kalshi_series_btts: None,
            poly_series_id: Some("10427"),
            kalshi_web_slug: "call-of-duty-game",
        },
    ]
}

/// Get config for a specific league
pub fn get_league_config(league: &str) -> Option<LeagueConfig> {
    get_league_configs()
        .into_iter()
        .find(|c| c.league_code == league || c.poly_prefix == league)
}

/// Discovery refresh interval in minutes (default: 15, 0 = disabled)
pub fn discovery_interval_mins() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("DISCOVERY_INTERVAL_MINS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15)
    })
}

use std::collections::HashSet;
use trading::execution::Platform;

/// Parse CONTROLLER_PLATFORMS env var.
/// Returns empty set if not set (pure router mode).
pub fn parse_controller_platforms() -> HashSet<Platform> {
    std::env::var("CONTROLLER_PLATFORMS")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|p| match p.trim().to_lowercase().as_str() {
                    "kalshi" => Some(Platform::Kalshi),
                    "polymarket" | "poly" => Some(Platform::Polymarket),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Mutex to serialize tests that modify CONTROLLER_PLATFORMS env var
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_discovery_interval_returns_reasonable_value() {
        // Note: This tests that discovery_interval_mins() returns a valid value.
        // Since the value is cached with OnceLock, this test works reliably
        // only when DISCOVERY_INTERVAL_MINS is not set in the test environment.
        // The function returns the cached value (default 15 if not set).
        let result = discovery_interval_mins();
        // Verify it returns a reasonable interval (up to 24 hours in minutes)
        assert!(result <= 60 * 24, "Should return a reasonable interval");
    }

    #[test]
    fn test_parse_controller_platforms_empty() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("CONTROLLER_PLATFORMS");
        let platforms = parse_controller_platforms();
        assert!(platforms.is_empty());
    }

    #[test]
    fn test_parse_controller_platforms_kalshi() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("CONTROLLER_PLATFORMS", "kalshi");
        let platforms = parse_controller_platforms();
        assert_eq!(platforms.len(), 1);
        assert!(platforms.contains(&Platform::Kalshi));
        std::env::remove_var("CONTROLLER_PLATFORMS");
    }

    #[test]
    fn test_parse_controller_platforms_both() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("CONTROLLER_PLATFORMS", "kalshi,polymarket");
        let platforms = parse_controller_platforms();
        assert_eq!(platforms.len(), 2);
        assert!(platforms.contains(&Platform::Kalshi));
        assert!(platforms.contains(&Platform::Polymarket));
        std::env::remove_var("CONTROLLER_PLATFORMS");
    }

    #[test]
    fn test_extract_base_game_slug_moneyline() {
        // Moneyline slugs have team suffix
        assert_eq!(
            extract_base_game_slug("sea-pis-ata-2026-01-16-ata"),
            "sea-pis-ata-2026-01-16"
        );
        assert_eq!(
            extract_base_game_slug("epl-cfc-avl-2025-12-27-cfc"),
            "epl-cfc-avl-2025-12-27"
        );
    }

    #[test]
    fn test_extract_base_game_slug_total() {
        // Total slugs have -total-{line} suffix
        assert_eq!(
            extract_base_game_slug("sea-pis-ata-2026-01-16-total-3pt5"),
            "sea-pis-ata-2026-01-16"
        );
        assert_eq!(
            extract_base_game_slug("nba-lal-bos-2026-01-20-total-220pt5"),
            "nba-lal-bos-2026-01-20"
        );
    }

    #[test]
    fn test_extract_base_game_slug_spread() {
        // Spread slugs have -spread-{line} suffix
        assert_eq!(
            extract_base_game_slug("nfl-kc-buf-2026-01-19-spread-3pt5"),
            "nfl-kc-buf-2026-01-19"
        );
    }

    #[test]
    fn test_extract_base_game_slug_btts() {
        // BTTS slugs have -btts suffix
        assert_eq!(
            extract_base_game_slug("epl-cfc-avl-2025-12-27-btts"),
            "epl-cfc-avl-2025-12-27"
        );
    }

    #[test]
    fn test_extract_base_game_slug_draw() {
        // Draw slugs have -draw suffix
        assert_eq!(
            extract_base_game_slug("sea-pis-ata-2026-01-16-draw"),
            "sea-pis-ata-2026-01-16"
        );
    }

    #[test]
    fn test_extract_base_game_slug_no_suffix() {
        // Already a base slug - should return unchanged
        assert_eq!(
            extract_base_game_slug("sea-pis-ata-2026-01-16"),
            "sea-pis-ata-2026-01-16"
        );
    }

    #[test]
    fn test_build_polymarket_url_sports() {
        // Sports markets use /event/{base_slug} (suffixes stripped)
        let url = build_polymarket_url("seriea", "sea-pis-ata-2026-01-16-ata");
        assert_eq!(
            url,
            "https://polymarket.com/event/sea-pis-ata-2026-01-16"
        );
    }

    #[test]
    fn test_build_polymarket_url_esports() {
        // Esports markets use /event/{slug}
        let url = build_polymarket_url("cs2", "cs2-furia-9ine-2026-01-16");
        assert_eq!(
            url,
            "https://polymarket.com/event/cs2-furia-9ine-2026-01-16"
        );
    }

    #[test]
    fn test_build_polymarket_url_ligue1() {
        // Ligue 1 example
        let url = build_polymarket_url("ligue1", "fl1-psg-lil-2026-01-16-psg");
        assert_eq!(
            url,
            "https://polymarket.com/event/fl1-psg-lil-2026-01-16"
        );
    }
}