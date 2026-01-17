//! Intelligent market discovery and matching system.
//!
//! This module handles the discovery of matching markets between Kalshi and Polymarket,
//! with support for caching, incremental updates, and parallel processing.

use anyhow::Result;
use futures_util::{stream, StreamExt};
use governor::{Quota, RateLimiter, state::NotKeyed, clock::DefaultClock, middleware::NoOpMiddleware};
use regex::Regex;
use std::sync::LazyLock;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::cache::{TeamCache, team_search_terms};
use crate::config;
use crate::config::{LeagueConfig, get_league_configs, get_league_config};
use crate::kalshi::KalshiApiClient;
use crate::polymarket::GammaClient;
use crate::types::{MarketPair, MarketType, DiscoveryResult, KalshiMarket, KalshiEvent};

/// Max concurrent Gamma API requests
const GAMMA_CONCURRENCY: usize = 5;

/// Kalshi rate limit: 2 requests per second (very conservative - they rate limit aggressively)
/// Must be conservative because discovery runs many leagues/series in parallel
const KALSHI_RATE_LIMIT_PER_SEC: u32 = 2;

/// Max concurrent Kalshi API requests GLOBALLY across all leagues/series
/// This is the hard cap - prevents bursting even when rate limiter has tokens
const KALSHI_GLOBAL_CONCURRENCY: usize = 1;

/// Cache file path
const DISCOVERY_CACHE_PATH: &str = ".discovery_cache.json";

/// Cache TTL in seconds (2 hours - new markets appear every ~2 hours)
const CACHE_TTL_SECS: u64 = 2 * 60 * 60;

/// Task for parallel Gamma lookup
struct GammaLookupTask {
    event: Arc<KalshiEvent>,
    market: KalshiMarket,
    poly_slug: String,
    market_type: MarketType,
    league: String,
    kalshi_web_slug: String,
    debug_idx: usize,
    debug_series: String,
}

/// Type alias for Kalshi rate limiter
type KalshiRateLimiter = RateLimiter<NotKeyed, governor::state::InMemoryState, DefaultClock, NoOpMiddleware>;

/// Persistent cache for discovered market pairs
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveryCache {
    /// Unix timestamp when cache was created
    timestamp_secs: u64,
    /// Cached market pairs
    pairs: Vec<MarketPair>,
    /// Set of known Kalshi market tickers (for incremental updates)
    known_kalshi_tickers: Vec<String>,
}

impl DiscoveryCache {
    fn new(pairs: Vec<MarketPair>) -> Self {
        let known_kalshi_tickers: Vec<String> = pairs.iter()
            .map(|p| p.kalshi_market_ticker.to_string())
            .collect();
        Self {
            timestamp_secs: current_unix_secs(),
            pairs,
            known_kalshi_tickers,
        }
    }

    fn is_expired(&self) -> bool {
        let now = current_unix_secs();
        now.saturating_sub(self.timestamp_secs) > CACHE_TTL_SECS
    }

    fn age_secs(&self) -> u64 {
        current_unix_secs().saturating_sub(self.timestamp_secs)
    }

    fn has_ticker(&self, ticker: &str) -> bool {
        self.known_kalshi_tickers.iter().any(|t| t == ticker)
    }
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Market discovery and matching client for cross-platform market identification
pub struct DiscoveryClient {
    kalshi: Arc<KalshiApiClient>,
    gamma: Arc<GammaClient>,
    pub team_cache: Arc<TeamCache>,
    kalshi_limiter: Arc<KalshiRateLimiter>,
    kalshi_semaphore: Arc<Semaphore>,  // Global concurrency limit for Kalshi
    gamma_semaphore: Arc<Semaphore>,
}

impl DiscoveryClient {
    pub fn new(kalshi: KalshiApiClient, team_cache: TeamCache) -> Self {
        // Create token bucket rate limiter for Kalshi
        let quota = Quota::per_second(NonZeroU32::new(KALSHI_RATE_LIMIT_PER_SEC).unwrap());
        let kalshi_limiter = Arc::new(RateLimiter::direct(quota));

        Self {
            kalshi: Arc::new(kalshi),
            gamma: Arc::new(GammaClient::new()),
            team_cache: Arc::new(team_cache),
            kalshi_limiter,
            kalshi_semaphore: Arc::new(Semaphore::new(KALSHI_GLOBAL_CONCURRENCY)),
            gamma_semaphore: Arc::new(Semaphore::new(GAMMA_CONCURRENCY)),
        }
    }

    /// Load cache from disk (async)
    async fn load_cache() -> Option<DiscoveryCache> {
        let path = crate::paths::resolve_workspace_file(DISCOVERY_CACHE_PATH);
        let data = tokio::fs::read_to_string(&path).await.ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Save cache to disk (async)
    async fn save_cache(cache: &DiscoveryCache) -> Result<()> {
        let data = serde_json::to_string_pretty(cache)?;
        let path = crate::paths::resolve_workspace_file(DISCOVERY_CACHE_PATH);
        tokio::fs::write(&path, data).await?;
        Ok(())
    }
    
    /// Discover all market pairs with caching support
    ///
    /// Strategy:
    /// 1. Try to load cache from disk
    /// 2. If cache exists and is fresh (<2 hours), use it directly
    /// 3. If cache exists but is stale, load it + fetch incremental updates
    /// 4. If no cache, do full discovery
    pub async fn discover_all(&self, leagues: &[&str], market_type_filter: Option<MarketType>) -> DiscoveryResult {
        // Try to load existing cache
        let cached = Self::load_cache().await;

        match cached {
            Some(cache) if !cache.is_expired() && market_type_filter.is_none() => {
                // Cache is fresh and no filter - use it directly
                let age = cache.age_secs();
                let pairs = filter_pairs_by_leagues(cache.pairs, leagues);
                info!("📂 Loaded {} pairs from cache (age: {}s){}",
                      pairs.len(), age,
                      if !leagues.is_empty() { format!(" [filtered to {:?}]", leagues) } else { String::new() });
                return DiscoveryResult {
                    pairs,
                    kalshi_events_found: 0,  // From cache
                    poly_matches: 0,
                    poly_misses: 0,
                    errors: vec![],
                };
            }
            Some(cache) if market_type_filter.is_none() => {
                // Cache is stale - do incremental discovery
                info!("📂 Cache expired (age: {}s), doing incremental refresh...", cache.age_secs());
                return self.discover_incremental(leagues, cache, market_type_filter).await;
            }
            _ => {
                // No cache or market type filter specified - do full discovery
                if market_type_filter.is_some() {
                    info!("📂 Market type filter active, doing fresh discovery...");
                } else {
                    info!("📂 No cache found, doing full discovery...");
                }
            }
        }

        // Full discovery (no cache)
        let result = self.discover_full(leagues, market_type_filter).await;

        // Save to cache
        if !result.pairs.is_empty() {
            let cache = DiscoveryCache::new(result.pairs.clone());
            if let Err(e) = Self::save_cache(&cache).await {
                warn!("Failed to save discovery cache: {}", e);
            } else {
                info!("💾 Saved {} pairs to cache", result.pairs.len());
            }
        }

        result
    }

    /// Force full discovery (ignores cache)
    pub async fn discover_all_force(&self, leagues: &[&str], market_type_filter: Option<MarketType>) -> DiscoveryResult {
        info!("🔄 Forced full discovery (ignoring cache)...");
        let result = self.discover_full(leagues, market_type_filter).await;

        // Save to cache
        if !result.pairs.is_empty() {
            let cache = DiscoveryCache::new(result.pairs.clone());
            if let Err(e) = Self::save_cache(&cache).await {
                warn!("Failed to save discovery cache: {}", e);
            } else {
                info!("💾 Saved {} pairs to cache", result.pairs.len());
            }
        }

        result
    }

    /// Full discovery without cache
    async fn discover_full(&self, leagues: &[&str], market_type_filter: Option<MarketType>) -> DiscoveryResult {
        let configs: Vec<_> = if leagues.is_empty() {
            get_league_configs()
        } else {
            leagues.iter()
                .filter_map(|l| get_league_config(l))
                .collect()
        };

        // Parallel discovery across all leagues
        let league_futures: Vec<_> = configs.iter()
            .map(|config| self.discover_league(config, None, market_type_filter))
            .collect();

        let league_results = futures_util::future::join_all(league_futures).await;

        // Merge results
        let mut result = DiscoveryResult::default();
        for league_result in league_results {
            result.pairs.extend(league_result.pairs);
            result.poly_matches += league_result.poly_matches;
            result.errors.extend(league_result.errors);
        }
        result.kalshi_events_found = result.pairs.len();

        result
    }

    /// Incremental discovery - merge cached pairs with newly discovered ones
    async fn discover_incremental(&self, leagues: &[&str], cache: DiscoveryCache, market_type_filter: Option<MarketType>) -> DiscoveryResult {
        let configs: Vec<_> = if leagues.is_empty() {
            get_league_configs()
        } else {
            leagues.iter()
                .filter_map(|l| get_league_config(l))
                .collect()
        };

        // Discover with filter for known tickers
        let league_futures: Vec<_> = configs.iter()
            .map(|config| self.discover_league(config, Some(&cache), market_type_filter))
            .collect();

        let league_results = futures_util::future::join_all(league_futures).await;

        // Merge cached pairs with newly discovered ones
        let mut all_pairs = cache.pairs;
        let mut new_count = 0;

        for league_result in league_results {
            for pair in league_result.pairs {
                if !all_pairs.iter().any(|p| *p.kalshi_market_ticker == *pair.kalshi_market_ticker) {
                    all_pairs.push(pair);
                    new_count += 1;
                }
            }
        }

        if new_count > 0 {
            info!("🆕 Found {} new market pairs", new_count);

            // Update cache
            let new_cache = DiscoveryCache::new(all_pairs.clone());
            if let Err(e) = Self::save_cache(&new_cache).await {
                warn!("Failed to update discovery cache: {}", e);
            } else {
                info!("💾 Updated cache with {} total pairs", all_pairs.len());
            }
        } else {
            info!("✅ No new markets found, using {} cached pairs", all_pairs.len());

            // Just update timestamp to extend TTL
            let refreshed_cache = DiscoveryCache::new(all_pairs.clone());
            let _ = Self::save_cache(&refreshed_cache).await;
        }

        DiscoveryResult {
            pairs: all_pairs,
            kalshi_events_found: new_count,
            poly_matches: new_count,
            poly_misses: 0,
            errors: vec![],
        }
    }

    /// Discover all market types for a single league (PARALLEL)
    /// If cache is provided, only discovers markets not already in cache
    /// If market_type_filter is provided, only discovers that market type
    async fn discover_league(&self, config: &LeagueConfig, cache: Option<&DiscoveryCache>, market_type_filter: Option<MarketType>) -> DiscoveryResult {
        // Use esports discovery for leagues with poly_series_id
        if config.poly_series_id.is_some() {
            return self.discover_esports_league(config).await;
        }

        info!("🔍 Discovering {} markets...", config.league_code);

        let all_market_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];

        // Filter market types if filter is specified
        let market_types: Vec<_> = all_market_types.iter()
            .filter(|mt| market_type_filter.map_or(true, |f| f == **mt))
            .copied()
            .collect();

        // Parallel discovery across market types
        let type_futures: Vec<_> = market_types.iter()
            .filter_map(|market_type| {
                let series = self.get_series_for_type(config, *market_type)?;
                Some(self.discover_series(config, series, *market_type, cache))
            })
            .collect();

        let type_results = futures_util::future::join_all(type_futures).await;

        let mut result = DiscoveryResult::default();
        for (pairs_result, market_type) in type_results.into_iter().zip(market_types.iter()) {
            match pairs_result {
                Ok(pairs) => {
                    let count = pairs.len();
                    if count > 0 {
                        info!("  ✅ {} {}: {} pairs", config.league_code, market_type, count);
                    }
                    result.poly_matches += count;
                    result.pairs.extend(pairs);
                }
                Err(e) => {
                    result.errors.push(format!("{} {}: {}", config.league_code, market_type, e));
                }
            }
        }

        result
    }
    
    fn get_series_for_type(&self, config: &LeagueConfig, market_type: MarketType) -> Option<&'static str> {
        match market_type {
            MarketType::Moneyline => Some(config.kalshi_series_game),
            MarketType::Spread => config.kalshi_series_spread,
            MarketType::Total => config.kalshi_series_total,
            MarketType::Btts => config.kalshi_series_btts,
        }
    }
    
    /// Discover markets for a specific series (PARALLEL Kalshi + Gamma lookups)
    /// If cache is provided, skips markets already in cache
    async fn discover_series(
        &self,
        config: &LeagueConfig,
        series: &str,
        market_type: MarketType,
        cache: Option<&DiscoveryCache>,
    ) -> Result<Vec<MarketPair>> {
        let pairing_debug = config::pairing_debug_enabled();
        let pairing_debug_limit = config::pairing_debug_limit();

        // Fetch Kalshi events
        {
            let _permit = self.kalshi_semaphore.acquire().await.map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;
            self.kalshi_limiter.until_ready().await;
        }
        let events = self.kalshi.get_events(series, 50).await?;

        if events.is_empty() {
            return Ok(vec![]);
        }
        info!("  📡 {} {}: {} events from Kalshi", config.league_code, market_type, events.len());

        // PHASE 2: Parallel market fetching 
        let kalshi = self.kalshi.clone();
        let limiter = self.kalshi_limiter.clone();
        let semaphore = self.kalshi_semaphore.clone();

        // Parse events first, filtering out unparseable ones
        let parsed_events: Vec<_> = events.into_iter()
            .filter_map(|event| {
                let parsed = match parse_kalshi_event_ticker(&event.event_ticker) {
                    Some(p) => p,
                    None => {
                        warn!("  ⚠️ Could not parse event ticker {}", event.event_ticker);
                        return None;
                    }
                };
                Some((parsed, event))
            })
            .collect();
        let parsed_events_len = parsed_events.len();

        // Execute market fetches with GLOBAL concurrency limit
        let market_results: Vec<_> = stream::iter(parsed_events)
            .map(|(parsed, event)| {
                let kalshi = kalshi.clone();
                let limiter = limiter.clone();
                let semaphore = semaphore.clone();
                let event_ticker = event.event_ticker.clone();
                async move {
                    let _permit = semaphore.acquire().await.ok();
                    // rate limit
                    limiter.until_ready().await;
                    let markets_result = kalshi.get_markets(&event_ticker).await;
                    (parsed, Arc::new(event), markets_result)
                }
            })
            .buffer_unordered(KALSHI_GLOBAL_CONCURRENCY * 2)  // Allow some buffering, semaphore is the real limit
            .collect()
            .await;

        // Collect all (event, market) pairs
        let mut event_markets = Vec::with_capacity(market_results.len() * 3);
        let mut cached_count = 0usize;
        for (parsed, event, markets_result) in market_results {
            match markets_result {
                Ok(markets) => {
                    for market in markets {
                        // Skip if already in cache
                        if let Some(c) = cache {
                            if c.has_ticker(&market.ticker) {
                                cached_count += 1;
                                continue;
                            }
                        }
                        event_markets.push((parsed.clone(), event.clone(), market));
                    }
                }
                Err(e) => {
                    warn!("  ⚠️ Failed to get markets for {}: {}", event.event_ticker, e);
                }
            }
        }

        if event_markets.is_empty() {
            if cached_count > 0 {
                info!("  ✅ {} {}: {} markets (all cached)", config.league_code, market_type, cached_count);
            }
            return Ok(vec![]);
        }
        info!("  🔎 {} {}: looking up {} new markets on Polymarket{}",
              config.league_code, market_type, event_markets.len(),
              if cached_count > 0 { format!(" ({} cached)", cached_count) } else { String::new() });

        if pairing_debug {
            info!(
                "🔎 [PAIR] league={} type={:?} series={} kalshi_events={} new_markets={} cached={}",
                config.league_code,
                market_type,
                series,
                parsed_events_len,
                event_markets.len(),
                cached_count
            );
        }

        // Parallel Gamma lookups with semaphore
        let lookup_futures: Vec<_> = event_markets
            .into_iter()
            .enumerate()
            .map(|(idx, (parsed, event, market))| {
                let poly_slug = self.build_poly_slug(config.poly_prefix, &parsed, market_type, &market, config.has_draws(), config.home_team_first);

                if pairing_debug && idx < pairing_debug_limit {
                    info!(
                        "🧩 [PAIR] #{}/{} league={} type={:?} event={} market={} parsed=({},{},{}) slug={}",
                        idx + 1,
                        pairing_debug_limit,
                        config.league_code,
                        market_type,
                        event.event_ticker,
                        market.ticker,
                        parsed.date,
                        parsed.team1,
                        parsed.team2,
                        poly_slug
                    );
                }

                GammaLookupTask {
                    event,
                    market,
                    poly_slug,
                    market_type,
                    league: config.league_code.to_string(),
                    kalshi_web_slug: config.kalshi_web_slug.to_string(),
                    debug_idx: idx,
                    debug_series: series.to_string(),
                }
            })
            .collect();

        // Track stats with atomic counters for the async closure
        let total_tasks = lookup_futures.len();
        let miss_slug_not_found = Arc::new(AtomicUsize::new(0));
        let miss_api_error = Arc::new(AtomicUsize::new(0));

        // Execute lookups in parallel
        let pairs: Vec<MarketPair> = stream::iter(lookup_futures)
            .map(|task| {
                let gamma = self.gamma.clone();
                let semaphore = self.gamma_semaphore.clone();
                let miss_slug_not_found = miss_slug_not_found.clone();
                let miss_api_error = miss_api_error.clone();
                async move {
                    let _permit = semaphore.acquire().await.ok()?;
                    match gamma.lookup_market(&task.poly_slug).await {
                        Ok(Some((token1, token2, outcomes))) => {
                            let team_suffix = extract_team_suffix(&task.market.ticker);

                            // DIAGNOSTIC: Log API response for token-outcome verification
                            if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                info!(
                                    "🔍 [PAIR] API response: slug={} token1={}... token2={}... outcomes={:?}",
                                    task.poly_slug,
                                    &token1[..token1.len().min(12)],
                                    &token2[..token2.len().min(12)],
                                    outcomes
                                );
                            }

                            // Use outcomes to determine which token is YES for this Kalshi market
                            // outcomes[i] corresponds to token[i] from Gamma API
                            let (yes_token, no_token) = if let Some(suffix) = &team_suffix {
                                // First check: Is this a Yes/No or Over/Under market?
                                // These are team-specific markets where API order is always correct:
                                // - Moneyline: "Will MUN win?" → outcomes=["Yes", "No"]
                                // - Spread: "Will MUN cover -1.5?" → outcomes=["Yes", "No"]
                                // - Total: "Will game go over 2.5?" → outcomes=["Over", "Under"]
                                let is_yes_no_market = outcomes.iter().any(|o| {
                                    let lower = o.to_lowercase();
                                    lower == "yes" || lower == "no" || lower == "over" || lower == "under"
                                });

                                if is_yes_no_market {
                                    // For Yes/No markets, API order is: [Yes token, No token]
                                    // This is correct - token1 is always the "Yes" outcome
                                    if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                        info!(
                                            "✅ [PAIR] Yes/No market, using API order: league={} type={:?} slug={} suffix={} outcomes={:?}",
                                            task.league, task.market_type, task.poly_slug, suffix, outcomes
                                        );
                                    }
                                    (token1, token2)
                                } else {
                                    // Outcomes contain team names (e.g., ["Manchester United FC", "Everton FC"])
                                    // Need to match Kalshi suffix to determine which token is YES

                                    // Strip numeric suffix for spread/total markets (e.g., "LEE1" -> "LEE")
                                    let team_code: String = suffix.chars()
                                        .take_while(|c| c.is_alphabetic())
                                        .collect::<String>()
                                        .to_lowercase();

                                    // Check if outcome[0] contains the team code (team we're betting YES on)
                                    let outcome0_matches = outcomes.get(0)
                                        .map(|o| o.to_lowercase().contains(&team_code))
                                        .unwrap_or(false);
                                    let outcome1_matches = outcomes.get(1)
                                        .map(|o| o.to_lowercase().contains(&team_code))
                                        .unwrap_or(false);

                                    if outcome0_matches && !outcome1_matches {
                                        // token1 is YES for this team (outcome[0] contains team code)
                                        if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                            info!(
                                                "✅ [PAIR] league={} type={:?} slug={} suffix={} team_code={} → token1=YES (matched outcome[0]) outcomes={:?}",
                                                task.league, task.market_type, task.poly_slug, suffix, team_code, outcomes
                                            );
                                        }
                                        (token1, token2)
                                    } else if outcome1_matches && !outcome0_matches {
                                        // token2 is YES for this team (outcome[1] contains team code)
                                        if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                            info!(
                                                "✅ [PAIR] league={} type={:?} slug={} suffix={} team_code={} → token2=YES (matched outcome[1]) outcomes={:?}",
                                                task.league, task.market_type, task.poly_slug, suffix, team_code, outcomes
                                            );
                                        }
                                        (token2, token1)
                                    } else {
                                        // Direct substring match failed, try team_search_terms lookup
                                        let search_terms = team_search_terms(&task.league, &team_code);

                                        if let Some(terms) = search_terms {
                                            // Check if any search term matches in outcomes
                                            let outcome0_lower = outcomes.get(0).map(|o| o.to_lowercase()).unwrap_or_default();
                                            let outcome1_lower = outcomes.get(1).map(|o| o.to_lowercase()).unwrap_or_default();

                                            let term_matches_0 = terms.iter().any(|term| outcome0_lower.contains(term));
                                            let term_matches_1 = terms.iter().any(|term| outcome1_lower.contains(term));

                                            if term_matches_0 && !term_matches_1 {
                                                // token1 is YES (search terms matched outcome[0])
                                                if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                                    info!(
                                                        "✅ [PAIR] league={} type={:?} slug={} suffix={} team_code={} → token1=YES (matched via search terms {:?}) outcomes={:?}",
                                                        task.league, task.market_type, task.poly_slug, suffix, team_code, terms, outcomes
                                                    );
                                                }
                                                (token1, token2)
                                            } else if term_matches_1 && !term_matches_0 {
                                                // token2 is YES (search terms matched outcome[1])
                                                if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                                    info!(
                                                        "✅ [PAIR] league={} type={:?} slug={} suffix={} team_code={} → token2=YES (matched via search terms {:?}) outcomes={:?}",
                                                        task.league, task.market_type, task.poly_slug, suffix, team_code, terms, outcomes
                                                    );
                                                }
                                                (token2, token1)
                                            } else {
                                                // Still ambiguous even with search terms
                                                warn!(
                                                    "⚠️ [PAIR] AMBIGUOUS after search terms! league={} type={:?} slug={} suffix={} team_code={} terms={:?} outcomes={:?} → using API order (may be wrong!)",
                                                    task.league, task.market_type, task.poly_slug, suffix, team_code, terms, outcomes
                                                );
                                                (token1, token2)
                                            }
                                        } else {
                                            // No search terms available - THIS IS A BUG for team-name outcomes
                                            warn!(
                                                "⚠️ [PAIR] AMBIGUOUS TOKEN ASSIGNMENT! league={} type={:?} slug={} suffix={} team_code={} outcomes={:?} → using API order (may be wrong!)",
                                                task.league, task.market_type, task.poly_slug, suffix, team_code, outcomes
                                            );
                                            (token1, token2)
                                        }
                                    }
                                }
                            } else {
                                // No team suffix (shouldn't happen for moneyline), use API order
                                if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                    warn!(
                                        "⚠️ [PAIR] league={} type={:?} series={} slug={} no team_suffix → using API order outcomes={:?}",
                                        task.league, task.market_type, task.debug_series, task.poly_slug, outcomes
                                    );
                                }
                                (token1, token2)
                            };

                            if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                info!(
                                    "🎯 [PAIR] MATCH league={} type={:?} kalshi_market={} poly_slug={} yes_token={} no_token={}",
                                    task.league,
                                    task.market_type,
                                    task.market.ticker,
                                    task.poly_slug,
                                    yes_token,
                                    no_token
                                );
                            }

                            Some(MarketPair {
                                pair_id: format!("{}-{}", task.poly_slug, task.market.ticker).into(),
                                league: task.league.into(),
                                market_type: task.market_type,
                                description: format!("{} - {}", task.event.title, task.market.title).into(),
                                kalshi_event_ticker: task.event.event_ticker.clone().into(),
                                kalshi_market_ticker: task.market.ticker.into(),
                                kalshi_event_slug: task.kalshi_web_slug.into(),
                                poly_slug: task.poly_slug.into(),
                                poly_yes_token: yes_token.into(),
                                poly_no_token: no_token.into(),
                                line_value: task.market.floor_strike,
                                team_suffix: team_suffix.map(|s| s.into()),
                            })
                        }
                        Ok(None) => {
                            miss_slug_not_found.fetch_add(1, Ordering::Relaxed);
                            if config::pairing_debug_enabled() && task.debug_idx < config::pairing_debug_limit() {
                                info!(
                                    "❌ [PAIR] NO_MATCH league={} type={:?} series={} kalshi_market={} poly_slug={}",
                                    task.league, task.market_type, task.debug_series, task.market.ticker, task.poly_slug
                                );
                            }
                            None
                        }
                        Err(e) => {
                            miss_api_error.fetch_add(1, Ordering::Relaxed);
                            warn!("  ⚠️ Gamma lookup failed for {}: {}", task.poly_slug, e);
                            None
                        }
                    }
                }
            })
            .buffer_unordered(GAMMA_CONCURRENCY)
            .filter_map(|x| async { x })
            .collect()
            .await;

        // Log summary stats when pairing_debug is enabled
        let matched = pairs.len();
        let slug_misses = miss_slug_not_found.load(Ordering::Relaxed);
        let api_errors = miss_api_error.load(Ordering::Relaxed);

        if pairing_debug && total_tasks > 0 {
            let match_rate = (matched as f64 / total_tasks as f64 * 100.0).round() as u32;
            let mut reasons = Vec::new();
            if slug_misses > 0 {
                reasons.push(format!("poly_no_market={}", slug_misses));
            }
            if api_errors > 0 {
                reasons.push(format!("api_error={}", api_errors));
            }
            let reasons_str = if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", reasons.join(", "))
            };
            info!(
                "📊 [PAIR] SUMMARY league={} type={:?} total={} matched={} ({}%) misses={}{}",
                config.league_code, market_type, total_tasks, matched, match_rate,
                total_tasks - matched, reasons_str
            );
        } else if !pairs.is_empty() {
            info!("  ✅ {} {}: matched {} pairs", config.league_code, market_type, pairs.len());
        }

        Ok(pairs)
    }
    
    /// Build Polymarket slug from Kalshi event data
    ///
    /// `has_draws` indicates whether the league has draw outcomes (soccer).
    /// - For leagues with draws: Moneyline uses team-specific slugs (e.g., `epl-cfc-avl-2025-12-27-cfc`)
    /// - For leagues without draws (NBA, NFL, etc.): Moneyline uses base slug (e.g., `nba-was-sac-2026-01-16`)
    ///
    /// `home_team_first` indicates the team order in Kalshi tickers:
    /// - true (soccer): HOME-AWAY order (team1=home, team2=away)
    /// - false (US sports): AWAY-HOME order (team1=away, team2=home)
    fn build_poly_slug(
        &self,
        poly_prefix: &str,
        parsed: &ParsedKalshiTicker,
        market_type: MarketType,
        market: &KalshiMarket,
        has_draws: bool,
        home_team_first: bool,
    ) -> String {
        // Convert Kalshi team codes to Polymarket codes using cache
        let poly_team1 = self.team_cache
            .kalshi_to_poly(poly_prefix, &parsed.team1)
            .unwrap_or_else(|| parsed.team1.to_lowercase());
        let poly_team2 = self.team_cache
            .kalshi_to_poly(poly_prefix, &parsed.team2)
            .unwrap_or_else(|| parsed.team2.to_lowercase());

        // Convert date from "25DEC27" to "2025-12-27"
        let date_str = kalshi_date_to_iso(&parsed.date);

        // Base slug: league-team1-team2-date
        let base = format!("{}-{}-{}-{}", poly_prefix, poly_team1, poly_team2, date_str);

        match market_type {
            MarketType::Moneyline => {
                if let Some(suffix) = extract_team_suffix(&market.ticker) {
                    if suffix.to_lowercase() == "tie" {
                        // Draw market - append -draw suffix
                        format!("{}-draw", base)
                    } else if has_draws {
                        // Soccer: uses team-specific slugs for each outcome
                        let poly_suffix = self.team_cache
                            .kalshi_to_poly(poly_prefix, &suffix)
                            .unwrap_or_else(|| suffix.to_lowercase());
                        format!("{}-{}", base, poly_suffix)
                    } else {
                        // American sports: single market with base slug
                        // Polymarket uses one market with YES/NO tokens for each team
                        base.clone()
                    }
                } else {
                    base
                }
            }
            MarketType::Spread => {
                // Polymarket uses "spread-home-{value}" when home team is favored,
                // "spread-away-{value}" when away team is favored.
                //
                // Team order in Kalshi tickers varies by league:
                // - Soccer (home_team_first=true): team1=HOME, team2=AWAY
                // - US Sports (home_team_first=false): team1=AWAY, team2=HOME
                let spread_type = if let Some(suffix) = extract_team_suffix(&market.ticker) {
                    // Extract team code from suffix (e.g., "DEN12" -> "DEN", "CRY1" -> "CRY")
                    let team_code: String = suffix.chars().take_while(|c| c.is_alphabetic()).collect();
                    let suffix_matches_team1 = team_code.eq_ignore_ascii_case(&parsed.team1);

                    if home_team_first {
                        // Soccer: team1=home, team2=away
                        if suffix_matches_team1 {
                            "spread-home" // suffix matches home team (team1)
                        } else {
                            "spread-away" // suffix matches away team (team2)
                        }
                    } else {
                        // US Sports: team1=away, team2=home
                        if suffix_matches_team1 {
                            "spread-away" // suffix matches away team (team1)
                        } else {
                            "spread-home" // suffix matches home team (team2)
                        }
                    }
                } else {
                    "spread-home"
                };

                if let Some(floor) = market.floor_strike {
                    let floor_str = format!("{:.1}", floor).replace(".", "pt");
                    format!("{}-{}-{}", base, spread_type, floor_str)
                } else {
                    format!("{}-{}", base, spread_type)
                }
            }
            MarketType::Total => {
                if let Some(floor) = market.floor_strike {
                    let floor_str = format!("{:.1}", floor).replace(".", "pt");
                    format!("{}-total-{}", base, floor_str)
                } else {
                    format!("{}-total", base)
                }
            }
            MarketType::Btts => {
                format!("{}-btts", base)
            }
        }
    }

    /// Discover new markets created since a given timestamp
    /// Returns only NEW pairs not already in the known_tickers set
    pub async fn discover_since(
        &self,
        since_ts: u64,
        known_tickers: &std::collections::HashSet<String>,
        leagues: &[&str],
    ) -> DiscoveryResult {
        let configs: Vec<_> = if leagues.is_empty() {
            get_league_configs()
        } else {
            leagues.iter()
                .filter_map(|l| get_league_config(l))
                .collect()
        };

        let mut result = DiscoveryResult::default();

        for config in &configs {
            match self.discover_series_since(config, since_ts, known_tickers).await {
                Ok(pairs) => {
                    if !pairs.is_empty() {
                        tracing::info!("  {} {}: {} new pairs",
                            config.league_code, "discovery", pairs.len());
                    }
                    result.pairs.extend(pairs);
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", config.league_code, e));
                }
            }
        }

        result.kalshi_events_found = result.pairs.len();
        result.poly_matches = result.pairs.len();
        result
    }

    /// Discover new markets for a single league since timestamp
    async fn discover_series_since(
        &self,
        config: &LeagueConfig,
        since_ts: u64,
        known_tickers: &std::collections::HashSet<String>,
    ) -> Result<Vec<MarketPair>> {
        let mut all_pairs = Vec::new();

        // Check all series for this league
        let series_list: Vec<&str> = [
            Some(config.kalshi_series_game),
            config.kalshi_series_spread,
            config.kalshi_series_total,
            config.kalshi_series_btts,
        ].into_iter().flatten().collect();

        for series in series_list {
            // Rate limit
            {
                let _permit = self.kalshi_semaphore.acquire().await
                    .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;
                self.kalshi_limiter.until_ready().await;
            }

            let markets = match self.kalshi.get_markets_since(series, since_ts).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("  Failed to query {}: {}", series, e);
                    continue;
                }
            };

            // Filter to only new markets
            let new_markets: Vec<_> = markets.into_iter()
                .filter(|m| !known_tickers.contains(&m.ticker))
                .collect();

            if new_markets.is_empty() {
                continue;
            }

            // Look up on Polymarket in parallel
            let pairs: Vec<MarketPair> = stream::iter(new_markets)
                .map(|market| {
                    async move {
                        self.try_match_market(config, &market).await
                    }
                })
                .buffer_unordered(GAMMA_CONCURRENCY)
                .filter_map(|x| async { x })
                .collect()
                .await;

            all_pairs.extend(pairs);
        }

        Ok(all_pairs)
    }

    /// Try to match a single Kalshi market to Polymarket
    async fn try_match_market(&self, config: &LeagueConfig, market: &KalshiMarket) -> Option<MarketPair> {
        let pairing_debug = config::pairing_debug_enabled();

        // Extract event ticker from market ticker (format: SERIES-EVENTID-SUFFIX)
        let parts: Vec<&str> = market.ticker.split('-').collect();
        if parts.len() < 2 {
            if pairing_debug {
                info!("❌ [PAIR] cannot split market ticker: {}", market.ticker);
            }
            return None;
        }

        // Reconstruct event ticker (SERIES-EVENTID)
        let event_ticker = format!("{}-{}", parts[0], parts[1]);

        // Determine market type from series
        let market_type = if market.ticker.contains("SPREAD") {
            MarketType::Spread
        } else if market.ticker.contains("TOTAL") {
            MarketType::Total
        } else if market.ticker.contains("BTTS") {
            MarketType::Btts
        } else {
            MarketType::Moneyline
        };

        // Parse event ticker to get teams and date
        let parsed = match parse_kalshi_event_ticker(&event_ticker) {
            Some(p) => p,
            None => {
                if pairing_debug {
                    info!(
                        "❌ [PAIR] cannot parse event_ticker={} (market={})",
                        event_ticker, market.ticker
                    );
                }
                return None;
            }
        };

        // Build poly slug
        let poly_slug = self.build_poly_slug(config.poly_prefix, &parsed, market_type, market, config.has_draws(), config.home_team_first);
        if pairing_debug {
            info!(
                "🧩 [PAIR] league={} type={:?} event={} market={} parsed=({},{},{}) slug={}",
                config.league_code,
                market_type,
                event_ticker,
                market.ticker,
                parsed.date,
                parsed.team1,
                parsed.team2,
                poly_slug
            );
        }

        // Look up on Polymarket
        let _permit = self.gamma_semaphore.acquire().await.ok()?;
        let (token1, token2, outcomes) = match self.gamma.lookup_market(&poly_slug).await {
            Ok(Some(result)) => result,
            Ok(None) => {
                if pairing_debug {
                    info!(
                        "❌ [PAIR] NO_MATCH league={} type={:?} market={} poly_slug={}",
                        config.league_code, market_type, market.ticker, poly_slug
                    );
                }
                return None;
            }
            Err(e) => {
                tracing::warn!("  ⚠️ Gamma lookup failed for {}: {}", poly_slug, e);
                return None;
            }
        };

        let team_suffix = extract_team_suffix(&market.ticker);

        // Use outcomes to determine which token is YES for this Kalshi market
        let (yes_token, no_token) = if let Some(ref suffix) = team_suffix {
            let suffix_lower = suffix.to_lowercase();
            let outcome0_matches = outcomes.get(0)
                .map(|o| o.to_lowercase().contains(&suffix_lower))
                .unwrap_or(false);
            let outcome1_matches = outcomes.get(1)
                .map(|o| o.to_lowercase().contains(&suffix_lower))
                .unwrap_or(false);

            if outcome0_matches && !outcome1_matches {
                if pairing_debug {
                    info!(
                        "✅ [PAIR] slug={} suffix={} → token1=YES outcomes={:?}",
                        poly_slug, suffix, outcomes
                    );
                } else {
                    debug!("  🎯 {} | outcomes={:?} | suffix={} matches outcome[0] → token1=YES",
                           poly_slug, outcomes, suffix);
                }
                (token1, token2)
            } else if outcome1_matches && !outcome0_matches {
                if pairing_debug {
                    info!(
                        "✅ [PAIR] slug={} suffix={} → token2=YES outcomes={:?}",
                        poly_slug, suffix, outcomes
                    );
                } else {
                    debug!("  🎯 {} | outcomes={:?} | suffix={} matches outcome[1] → token2=YES",
                           poly_slug, outcomes, suffix);
                }
                (token2, token1)
            } else {
                if pairing_debug {
                    warn!(
                        "⚠️ [PAIR] slug={} suffix={} ambiguous outcomes={:?} → using API order",
                        poly_slug, suffix, outcomes
                    );
                } else {
                    warn!("  ⚠️ {} | outcomes={:?} | suffix={} - ambiguous match, using API order",
                          poly_slug, outcomes, suffix);
                }
                (token1, token2)
            }
        } else {
            if pairing_debug {
                warn!(
                    "⚠️ [PAIR] slug={} no team_suffix → using API order outcomes={:?}",
                    poly_slug, outcomes
                );
            } else {
                warn!("  ⚠️ {} | no team_suffix extracted, using API order", poly_slug);
            }
            (token1, token2)
        };

        if pairing_debug {
            info!(
                "🎯 [PAIR] MATCH league={} type={:?} kalshi_market={} poly_slug={} yes_token={} no_token={}",
                config.league_code,
                market_type,
                market.ticker,
                poly_slug,
                yes_token,
                no_token
            );
        }

        Some(MarketPair {
            pair_id: format!("{}-{}", poly_slug, market.ticker).into(),
            league: config.league_code.into(),
            market_type,
            description: market.title.to_string().into(),
            kalshi_event_ticker: event_ticker.into(),
            kalshi_market_ticker: market.ticker.clone().into(),
            kalshi_event_slug: config.kalshi_web_slug.into(),
            poly_slug: poly_slug.into(),
            poly_yes_token: yes_token.into(),
            poly_no_token: no_token.into(),
            line_value: market.floor_strike,
            team_suffix: team_suffix.map(|s| s.into()),
        })
    }

    /// Discover esports market pairs using series-based name matching
    async fn discover_esports_league(&self, config: &LeagueConfig) -> DiscoveryResult {
        let series_id = match config.poly_series_id {
            Some(id) => id,
            None => return DiscoveryResult::default(),
        };
        let pairing_debug = config::pairing_debug_enabled();

        info!("🎮 Discovering {} esports markets (series_id={})...", config.league_code, series_id);

        // Phase 1: Build Polymarket lookup from events
        let poly_events = match self.gamma.fetch_events_by_series(series_id).await {
            Ok(events) => events,
            Err(e) => {
                warn!("Failed to fetch Polymarket events for {}: {}", config.league_code, e);
                return DiscoveryResult {
                    errors: vec![format!("{}: {}", config.league_code, e)],
                    ..Default::default()
                };
            }
        };

        // Build lookup: (date:norm_team1:norm_team2) -> (slug, yes_token, no_token, poly_team1)
        // poly_team1 is the normalized name of the team that the YES token represents
        let mut poly_lookup: HashMap<String, (String, String, String, String)> = HashMap::new();

        for event in &poly_events {
            let slug = match &event.slug {
                Some(s) => s,
                None => continue,
            };

            let title = match &event.title {
                Some(t) => t,
                None => continue,
            };

            if let Some((team1, team2)) = parse_poly_event_title(title) {
                if let Some(date) = extract_date_from_poly_slug(slug) {
                    let norm1 = normalize_esports_team(&team1);
                    let norm2 = normalize_esports_team(&team2);

                    // Find moneyline market (no -game, -total, -map suffix)
                    if let Some(markets) = &event.markets {
                        for market in markets {
                            let market_slug = market.slug.as_deref().unwrap_or("");
                            let is_moneyline = !market_slug.contains("-game")
                                && !market_slug.contains("-total")
                                && !market_slug.contains("-map-")
                                && !market_slug.contains("-handicap");

                            if is_moneyline {
                                if let (Some(tokens), Some(outcomes_str)) = (&market.clob_token_ids, &market.outcomes) {
                                    if let (Ok(ids), Ok(outcomes)) = (
                                        serde_json::from_str::<Vec<String>>(tokens),
                                        serde_json::from_str::<Vec<String>>(outcomes_str)
                                    ) {
                                        if ids.len() >= 2 && outcomes.len() >= 2 {
                                            // Use canonical team lookup to match outcomes to title teams
                                            // outcomes[i] corresponds to ids[i]
                                            // Use RAW names for canonical lookup, not normalized
                                            let outcome0_matches_team1 = teams_match_canonical(&outcomes[0], &team1);
                                            let outcome1_matches_team1 = teams_match_canonical(&outcomes[1], &team1);
                                            let outcome0_matches_team2 = teams_match_canonical(&outcomes[0], &team2);
                                            let outcome1_matches_team2 = teams_match_canonical(&outcomes[1], &team2);

                                            // For logging/storage, still normalize
                                            let outcome0_norm = normalize_esports_team(&outcomes[0]);
                                            let outcome1_norm = normalize_esports_team(&outcomes[1]);

                                            let (team1_token, team2_token, poly_team1_norm) =
                                                if outcome0_matches_team1 || outcome1_matches_team2 {
                                                    // outcomes[0] is team1, outcomes[1] is team2
                                                    (ids[0].clone(), ids[1].clone(), outcome0_norm.clone())
                                                } else if outcome1_matches_team1 || outcome0_matches_team2 {
                                                    // outcomes[1] is team1, outcomes[0] is team2
                                                    (ids[1].clone(), ids[0].clone(), outcome1_norm.clone())
                                                } else {
                                                    // Fallback: use title order (norm1 first)
                                                    warn!("  ⚠️ Could not match outcomes {:?} to teams {}/{} via canonical lookup",
                                                          outcomes, team1, team2);
                                                    (ids[0].clone(), ids[1].clone(), outcome0_norm.clone())
                                                };

                                            if pairing_debug {
                                                info!("  🔍 {} | outcomes={:?} | o0_t1={} o1_t1={} o0_t2={} o1_t2={} | team1_tok_owner={} | title={}/{}",
                                                      slug, outcomes, outcome0_matches_team1, outcome1_matches_team1,
                                                      outcome0_matches_team2, outcome1_matches_team2, poly_team1_norm, team1, team2);
                                            }

                                            // Store with both key orderings
                                            let key1 = format!("{}:{}:{}", date, norm1, norm2);
                                            let key2 = format!("{}:{}:{}", date, norm2, norm1);
                                            poly_lookup.insert(key1, (slug.clone(), team1_token.clone(), team2_token.clone(), poly_team1_norm.clone()));
                                            poly_lookup.insert(key2, (slug.clone(), team1_token, team2_token, poly_team1_norm));
                                        }
                                    }
                                } else {
                                    warn!("  ⚠️ {} missing outcomes field (tokens={:?}, outcomes={:?})",
                                          slug, market.clob_token_ids.is_some(), market.outcomes.is_some());
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        info!("  📊 Built {} Polymarket lookup entries", poly_lookup.len() / 2);

        // Phase 2: Fetch and match Kalshi events
        let kalshi_events = {
            let _permit = self.kalshi_semaphore.acquire().await.ok();
            self.kalshi_limiter.until_ready().await;
            match self.kalshi.get_events(config.kalshi_series_game, 50).await {
                Ok(events) => events,
                Err(e) => {
                    warn!("Failed to fetch Kalshi events for {}: {}", config.league_code, e);
                    return DiscoveryResult {
                        errors: vec![format!("{}: {}", config.league_code, e)],
                        ..Default::default()
                    };
                }
            }
        };

        let mut pairs = Vec::new();

        for event in &kalshi_events {
            if let Some((team1, team2)) = parse_esports_kalshi_title(&event.title) {
                if let Some(date) = parse_kalshi_event_ticker(&event.event_ticker)
                    .map(|p| kalshi_date_to_iso(&p.date))
                {
                    let norm1 = normalize_esports_team(&team1);
                    let norm2 = normalize_esports_team(&team2);
                    let key = format!("{}:{}:{}", date, norm1, norm2);

                    if let Some((slug, yes_token, no_token, poly_team1)) = poly_lookup.get(&key) {
                        // Get Kalshi markets for this event
                        let markets = {
                            let _permit = self.kalshi_semaphore.acquire().await.ok();
                            self.kalshi_limiter.until_ready().await;
                            self.kalshi.get_markets(&event.event_ticker).await.unwrap_or_default()
                        };

                        for market in markets {
                            let team_suffix = extract_team_suffix(&market.ticker);

                            // Determine correct token assignment based on which team this Kalshi market is for
                            // Polymarket YES token = poly_team1 wins
                            // Polymarket NO token = poly_team1 loses (other team wins)
                            //
                            // If Kalshi market is for poly_team1: use tokens as-is
                            // If Kalshi market is for the other team: swap tokens
                            let kalshi_team_norm = team_suffix
                                .as_ref()
                                .map(|s| normalize_esports_team(s))
                                .unwrap_or_default();

                            // Check if kalshi_team matches poly_team1 using canonical lookup
                            // "tes" should match "top-esports", "wb" should match "weibo-gaming"
                            let is_match = teams_match_canonical(&kalshi_team_norm, poly_team1);
                            let swapped = !is_match;
                            let (poly_yes, poly_no) = if !swapped {
                                // Kalshi "Will Team1 win?" → Poly YES = Team1 wins, Poly NO = Team1 loses
                                (yes_token.clone(), no_token.clone())
                            } else {
                                // Kalshi "Will Team2 win?" → need to swap:
                                // Poly NO = Team1 loses = Team2 wins (this is our Kalshi YES equivalent)
                                // Poly YES = Team1 wins = Team2 loses (this is our Kalshi NO equivalent)
                                (no_token.clone(), yes_token.clone())
                            };

                            pairs.push(MarketPair {
                                pair_id: format!("{}-{}", slug, market.ticker).into(),
                                league: config.league_code.into(),
                                market_type: MarketType::Moneyline,
                                description: format!("{} - {}", event.title, market.title).into(),
                                kalshi_event_ticker: event.event_ticker.clone().into(),
                                kalshi_market_ticker: market.ticker.into(),
                                kalshi_event_slug: config.kalshi_web_slug.into(),
                                poly_slug: slug.clone().into(),
                                poly_yes_token: poly_yes.into(),
                                poly_no_token: poly_no.into(),
                                line_value: market.floor_strike,
                                team_suffix: team_suffix.map(|s| s.into()),
                            });
                        }
                    }
                }
            }
        }

        if !pairs.is_empty() {
            info!("  ✅ {} {}: matched {} pairs", config.league_code, "esports", pairs.len());
        }

        DiscoveryResult {
            pairs,
            kalshi_events_found: kalshi_events.len(),
            poly_matches: poly_lookup.len() / 2,
            poly_misses: 0,
            errors: vec![],
        }
    }
}

// === Helpers ===

/// Filter market pairs by enabled leagues
/// If leagues is empty, returns all pairs (no filtering)
fn filter_pairs_by_leagues(pairs: Vec<MarketPair>, leagues: &[&str]) -> Vec<MarketPair> {
    if leagues.is_empty() {
        return pairs;
    }
    pairs.into_iter()
        .filter(|p| leagues.iter().any(|l| *l == &*p.league))
        .collect()
}

#[derive(Debug, Clone)]
struct ParsedKalshiTicker {
    date: String,  // "25DEC27"
    team1: String, // "CFC"
    team2: String, // "AVL"
}

/// Parse Kalshi event ticker like "KXEPLGAME-25DEC27CFCAVL" or "KXNCAAFGAME-25DEC27M-OHFRES"
fn parse_kalshi_event_ticker(ticker: &str) -> Option<ParsedKalshiTicker> {
    let parts: Vec<&str> = ticker.split('-').collect();
    if parts.len() < 2 {
        return None;
    }

    // Handle two formats:
    // 1. "KXEPLGAME-25DEC27CFCAVL" - date+teams in parts[1]
    // 2. "KXNCAAFGAME-25DEC27M-OHFRES" - date in parts[1], teams in parts[2]
    let (date, teams_part) = if parts.len() >= 3 && parts[2].len() >= 4 {
        // Format 2: 3-part ticker with separate teams section
        // parts[1] is like "25DEC27M" (date + optional suffix)
        let date_part = parts[1];
        let date = if date_part.len() >= 7 {
            date_part[..7].to_uppercase()
        } else {
            return None;
        };
        (date, parts[2])
    } else {
        // Format 1: 2-part ticker with combined date+teams
        let date_teams = parts[1];
        // Minimum: 7 (date) + 2 + 2 (min team codes) = 11
        if date_teams.len() < 11 {
            return None;
        }
        let date = date_teams[..7].to_uppercase();
        let teams = &date_teams[7..];
        (date, teams)
    };

    // Split team codes - try to find the best split point
    // Team codes range from 2-4 chars (e.g., OM, CFC, FRES)
    let (team1, team2) = split_team_codes(teams_part);

    Some(ParsedKalshiTicker { date, team1, team2 })
}

/// Split a combined team string into two team codes
/// Tries multiple split strategies based on string length
fn split_team_codes(teams: &str) -> (String, String) {
    let len = teams.len();

    // For 6 chars, could be 3+3, 2+4, or 4+2
    // For 5 chars, could be 2+3 or 3+2
    // For 4 chars, must be 2+2
    // For 7 chars, could be 3+4 or 4+3
    // For 8 chars, could be 4+4, 3+5, 5+3

    match len {
        4 => (teams[..2].to_uppercase(), teams[2..].to_uppercase()),
        5 => {
            // Could be 2+3 (SJ+FLA, OM+ASM) or 3+2 (CAR+NJ, ANA+LA)
            let first_two = teams[..2].to_uppercase();
            let last_two = teams[3..].to_uppercase();

            // Check prefix first - if first 2 chars are a known code, use 2+3
            // (handles SJFLA = SJ+FLA where both SJ and LA are valid codes)
            if is_likely_two_letter_code(&first_two) {
                (first_two, teams[2..].to_uppercase())
            } else if is_known_two_letter_suffix(&last_two) {
                // Otherwise check if last 2 chars are a known suffix (3+2 pattern)
                // (handles CARNJ = CAR+NJ, ANALA = ANA+LA)
                (teams[..3].to_uppercase(), last_two)
            } else {
                // Default to 3+2 for unknown patterns (most sports use 3-letter codes)
                (teams[..3].to_uppercase(), teams[3..].to_uppercase())
            }
        }
        6 => {
            // Check if it looks like 2+4 pattern (e.g., OHFRES = OH+FRES)
            // Common 2-letter codes: OM, OL, OH, SF, LA, NY, KC, TB, etc.
            let first_two = &teams[..2].to_uppercase();
            if is_likely_two_letter_code(first_two) {
                (first_two.clone(), teams[2..].to_uppercase())
            } else {
                // Default to 3+3
                (teams[..3].to_uppercase(), teams[3..].to_uppercase())
            }
        }
        7 => {
            // Could be 3+4 or 4+3 - prefer 3+4
            (teams[..3].to_uppercase(), teams[3..].to_uppercase())
        }
        _ if len >= 8 => {
            // 4+4 or longer
            (teams[..4].to_uppercase(), teams[4..].to_uppercase())
        }
        _ => {
            let mid = len / 2;
            (teams[..mid].to_uppercase(), teams[mid..].to_uppercase())
        }
    }
}

/// Check if a 2-letter code is a known/likely team abbreviation
fn is_likely_two_letter_code(code: &str) -> bool {
    matches!(
        code,
        // European football (Ligue 1, etc.)
        "OM" | "OL" | "FC" |
        // Keep this list conservative: 2-letter codes are ambiguous in many leagues (e.g., NBA/NFL),
        // and mis-splitting breaks slug construction ("LACTOR" should be "LAC"+"TOR", not "LA"+"CTOR").
        // Only include codes we've seen appear as true 2-letter prefixes in Kalshi tickers.
        "OH" |
        // NHL 2-letter codes (can appear as first team, e.g., SJFLA = SJ+FLA)
        "SJ" | "TB" | "NJ" | "LA" |
        // NFL 2-letter codes (e.g., SFSEA = SF+SEA)
        "SF" |
        // Generic short codes
        "BC" | "SC" | "AC" | "AS" | "US"
    )
}

/// Check if a code is a known 2-letter team code that appears at END of combined strings
/// These are codes like NJ, LA, TB, SJ that Kalshi uses for NHL teams
fn is_known_two_letter_suffix(code: &str) -> bool {
    matches!(
        code,
        // NHL 2-letter codes (appear as second team in tickers like CARNJ, ANALA)
        "NJ" | "LA" | "TB" | "SJ"
    )
}

/// Convert Kalshi date "25DEC27" to ISO "2025-12-27"
fn kalshi_date_to_iso(kalshi_date: &str) -> String {
    if kalshi_date.len() != 7 {
        return kalshi_date.to_string();
    }
    
    let year = format!("20{}", &kalshi_date[..2]);
    let month = match &kalshi_date[2..5].to_uppercase()[..] {
        "JAN" => "01", "FEB" => "02", "MAR" => "03", "APR" => "04",
        "MAY" => "05", "JUN" => "06", "JUL" => "07", "AUG" => "08",
        "SEP" => "09", "OCT" => "10", "NOV" => "11", "DEC" => "12",
        _ => "01",
    };
    let day = &kalshi_date[5..7];
    
    format!("{}-{}-{}", year, month, day)
}

/// Extract team suffix from market ticker (e.g., "KXEPLGAME-25DEC27CFCAVL-CFC" -> "CFC")
fn extract_team_suffix(ticker: &str) -> Option<String> {
    let mut splits = ticker.splitn(3, '-');
    splits.next()?; // series
    splits.next()?; // event
    splits.next().map(|s| s.to_uppercase())
}

// === Esports Discovery Helpers ===

// Static mapping of esports team canonical names to their known aliases/abbreviations
// This is more reliable than fuzzy matching for the limited set of esports teams
// Generated/updated via: cargo run --bin generate-aliases
static ESPORTS_TEAM_ALIASES: &[(&str, &[&str])] = &[
    // LPL (China LoL)
    ("top-esports", &["tes", "top", "top esports", "topesports"]),
    ("weibo-gaming", &["wb", "wbg", "weibo", "weibo gaming"]),
    ("jd-gaming", &["jdg", "jd", "jd gaming"]),
    ("bilibili-gaming", &["blg", "bilibili", "bilibili gaming"]),
    ("lng-esports", &["lng", "lng esports"]),
    ("rare-atom", &["ra", "ra1", "rare atom"]),
    ("anyone-legends", &["al", "anyone", "anyone legends"]),
    ("oh-my-god", &["omg", "oh my god"]),
    ("funplus-phoenix", &["fpx", "funplus", "funplus phoenix"]),
    ("edward-gaming", &["edg", "edward", "edward gaming"]),
    ("royal-never-give-up", &["rng", "royal", "royal never give up"]),
    ("ninjas-in-pyjamas", &["nip", "ninjas", "ninjas in pyjamas"]),
    ("tt-gaming", &["tt", "tt gaming"]),
    ("ultra-prime", &["up", "ultra prime"]),
    ("invictus-gaming", &["ig", "ig1", "invictus", "invictus gaming"]),
    ("thundertalk-gaming", &["tt", "thundertalk", "thundertalk gaming"]),

    // LCK (Korea LoL)
    ("t1", &["t1", "skt", "sk telecom"]),
    ("gen-g", &["gen", "geng", "gen g", "gen.g"]),
    ("hanwha-life", &["hle", "hle1", "hanwha", "hanwha life"]),
    ("dplus-kia", &["dk", "dwg", "dplus", "dplus kia", "damwon"]),
    ("kt-rolster", &["kt", "ktr", "kt rolster"]),
    ("drx", &["drx"]),
    ("kwangdong-freecs", &["kdf", "kwangdong", "freecs"]),
    ("nongshim-redforce", &["ns", "nongshim", "redforce"]),
    ("ok-brion", &["bro", "bro2", "brion", "ok brion"]),
    ("fearx", &["fox", "fox1", "fearx"]),
    ("dn-soopers", &["dnf", "dn soopers"]),
    ("hanjin-brion", &["bro2", "hanjin", "hanjin brion"]),

    // LEC (Europe LoL)
    ("g2-esports", &["g2", "g2 esports"]),
    ("fnatic", &["fnc", "fnatic"]),
    ("mad-lions", &["mad", "mad lions"]),
    ("team-vitality", &["vit", "vitality", "team vitality"]),
    ("team-heretics", &["th", "heretics", "team heretics"]),
    ("sk-gaming", &["sk", "sk gaming"]),
    ("giantx", &["gx", "giantx", "giant x"]),
    ("karmine-corp", &["kc", "karmine", "karmine corp"]),
    ("karmine-corp-blue", &["kcb", "karmine corp blue"]),
    ("team-bds", &["bds", "team bds"]),
    ("rogue", &["rge", "rogue"]),
    ("los-ratones", &["lr", "los ratones"]),
    ("shifters", &["shft", "shifters"]),
    ("movistar-koi", &["mkoi", "movistar koi"]),

    // LCS (NA LoL)
    ("cloud9", &["c9", "cloud9", "cloud 9"]),
    ("team-liquid", &["tl", "tl1", "tl2", "tl-1", "tl 1", "liquid", "team liquid"]),
    ("flyquest", &["fly", "flyquest"]),
    ("100-thieves", &["100t", "100", "100 thieves"]),
    ("dignitas", &["dig", "dignitas"]),
    ("nrg-esports", &["nrg", "nrg esports"]),
    ("shopify-rebellion", &["sr", "shopify", "rebellion"]),
    ("immortals", &["imt", "immortals"]),
    ("sentinels", &["sen", "sentinels"]),
    ("disguised", &["dsg", "disguised"]),
    ("lyon", &["ly", "lyon"]),

    // CBLOL (Brazil LoL)
    ("furia", &["furia", "fur", "furia esports"]),
    ("loud", &["loud", "lll"]),
    ("pain-gaming", &["pain", "png1", "pain gaming"]),
    ("red-canids", &["red", "rc", "red canids"]),
    ("leviatan", &["lev", "leviatan"]),
    ("vivo-keyd-stars", &["vks", "vivo keyd stars"]),
    ("fluxo-w7m", &["fxw7", "fluxo w7m"]),
    ("los", &["los"]),

    // LCP (Pacific LoL)
    ("gam-esports", &["gam", "gam esports"]),
    ("ctbc-flying-oyster", &["cfo", "ctbc flying oyster"]),
    ("detonation-focusme", &["dfm", "detonation focusme"]),
    ("deep-cross-gaming", &["dcg", "deep cross gaming"]),
    ("ground-zero-gaming", &["gz", "ground zero", "ground zero gaming"]),
    ("fukuoka-softbank-hawks", &["shg", "fukuoka softbank hawks"]),
    ("team-secret-whales", &["tsw", "secret whales", "team secret whales"]),
    ("mvk-esports", &["mvk", "mvk esports"]),
    ("axolotl", &["axl", "axolotl"]),

    // TCL (Turkey LoL)
    ("dark-passage", &["dp", "dark passage"]),
    ("team-phoenix", &["phx2", "phoenix", "team phoenix"]),
    ("boostgate-esports", &["bgt", "boostgate"]),
    ("s2g-esports", &["s2g", "s2g esports"]),
    ("misa-esports", &["me1", "mis", "misa", "misa esports"]),
    ("su-esports", &["su", "su esports"]),
    ("bushido-wildcats", &["bw", "bushido wildcats"]),
    ("pcific", &["pcf", "pcific"]),

    // Other LoL
    ("colossal-gaming", &["cg", "colossal", "colossal gaming"]),
    ("eko-esports", &["eko", "eko esports"]),
    ("gmblers-esports", &["gmb", "gmblers"]),
    ("dragons-esports", &["drg", "dragons", "dragons esports"]),
    ("anubis-gaming", &["anb", "anubis", "anubis gaming"]),
    ("3bl-esports", &["3bl", "3bl esports"]),
    ("baam-esports", &["baam", "baam esports"]),

    // CS2 Teams
    ("natus-vincere", &["navi", "natus vincere", "na'vi"]),
    ("faze-clan", &["faze", "faze clan"]),
    ("vitality", &["vit", "vitality", "team vitality"]),
    ("heroic", &["heroic", "hero"]),
    ("mouz", &["mouz", "mousesports"]),
    ("spirit", &["spirit", "ts7", "team spirit"]),
    ("virtus-pro", &["vp", "virtus", "virtus pro", "virtus.pro"]),
    ("astralis", &["ast", "ast10", "astralis"]),
    ("complexity", &["col", "complexity"]),
    ("liquid", &["tl", "tl1", "tl2", "tl-1", "tl 1", "liquid", "team liquid"]),
    ("gentle-mates", &["m8", "m-8", "m 8", "gentle mates", "gentle-mates", "gentlemates"]),
    ("eternal-fire", &["ef", "ef1", "eternal", "eternal fire"]),
    ("monte", &["monte", "mnte"]),
    ("big", &["big", "big5", "big clan"]),
    ("9ine", &["9ine", "9 ine"]),
    ("3dmax", &["3dmax", "3d max"]),
    ("apeks", &["apeks"]),
    ("betboom", &["betboom", "bb3", "bet boom"]),
    ("imperial", &["imp", "imp11", "imperial"]),
    ("mibr", &["mibr"]),
    ("wildcard", &["wc", "wildcard"]),
    ("boss", &["boss"]),
    ("falcons", &["falcons", "fal2"]),
    ("sangal", &["sangal"]),
    ("saw", &["saw"]),
    ("gamerlegion", &["gl", "gl1", "gamerlegion"]),
    ("aurora", &["aurora", "aur1"]),
    ("b8", &["b8"]),
    ("nemiga", &["nemiga"]),
    ("ecstatic", &["ecstatic"]),
    ("permitta", &["permitta"]),
    ("rebels", &["rebels"]),
    ("passion-ua", &["passion", "passion ua"]),
    ("tyloo", &["tyloo"]),
    ("lynn-vision", &["lv", "lvg", "lynn vision", "lynnvision"]),
    ("the-mongolz", &["mg", "mongolz", "the mongolz"]),
    ("nouns", &["nouns"]),
    ("legacy", &["legacy", "lgc"]),
    ("m80", &["m80"]),
    ("pgl", &["pgl"]),
    ("ence", &["ence"]),
    ("parivision", &["prv", "parivision"]),
    ("acend", &["ace1", "acend"]),
    ("og", &["og", "og1"]),
    ("inner-circle-esports", &["ice", "inner circle"]),
    ("eyeballers", &["eye", "eyeballers"]),
    ("fut-esports", &["fut", "fut esports"]),
    ("vasco-esports", &["vsc", "vasco", "vasco esports"]),
    ("prison-breakers", &["pb2", "prison breakers"]),
    ("sharks", &["shk", "sharks"]),
    ("gaimin-gladiators", &["gg5", "gaimin gladiators"]),
    ("fake-do-biru", &["fdb", "fake do biru"]),
    ("hyperspirit", &["hs2", "hyperspirit"]),
    ("aab-esport", &["aab", "aab esport"]),
    ("bebop", &["bbp", "bebop"]),
    ("state", &["sta6", "state"]),
    ("prestige", &["pre", "prestige"]),
    ("aimclub", &["aim1", "aimclub"]),
    ("infinite", &["inf6", "infinite"]),
    ("johnny-speeds", &["js1", "johnny speeds"]),
    ("washington", &["wsg", "washington"]),
    ("phantom", &["pha", "phantom"]),
    ("fingers-crossed", &["fin", "fingers crossed"]),
    ("brute", &["brute"]),
    ("genone", &["g1", "genone"]),
    ("los-kogutos", &["lk", "los kogutos"]),
    ("hindsight", &["hin", "hindsight"]),
    ("pigeons", &["pge", "pigeons"]),
    ("petardka", &["petard", "petardka"]),
    ("illwill", &["ill", "illwill"]),
    ("lilmix", &["lilmix"]),
    ("enrage", &["enr1", "enrage"]),
    ("haspers", &["hasp", "haspers"]),
    ("mana-esports", &["mana", "mana esports"]),
    ("phantom-academy", &["phanto", "phantom academy"]),
    ("oxuji-esports", &["oxuji", "oxuji esports"]),
    ("vp-future", &["vpf", "vp future"]),
    ("young-tigeres", &["ytigeres", "young tigeres"]),
    ("b8-academy", &["b8acad", "b8 academy"]),
    ("jumbo-team", &["jumbo", "jumbo team"]),
    ("gothboiclique", &["got1", "gothboiclique"]),
    ("sashi-academy", &["sashia", "sashi academy"]),
    ("aurora-young-blood", &["auryb", "aurora young blood"]),
    ("enjoy", &["enjoy"]),
    ("fc-famalicao", &["fcf", "fc famalicao"]),
    ("exsad-gaming", &["exs", "exsad", "exsad gaming"]),
    ("megoshort", &["meg1", "megoshort"]),
    ("semperfi-esports", &["sfe", "semperfi"]),
    ("steel-helmet", &["sh2", "steel helmet"]),
    ("team-xdm", &["xdm", "team xdm"]),
    ("morningstar", &["mornin", "morningstar"]),
    ("eruption", &["erupt", "eruption"]),
    ("jijiehao", &["jjh", "jijiehao"]),
    ("change-the-game", &["ctg", "change the game"]),
    ("the-huns-esports", &["the9", "the huns"]),
    ("just-swing", &["js", "just swing"]),
    ("unsettled-resentment", &["ur", "unsettled resentment"]),
    ("ever-growing", &["eg4", "ever growing"]),
    ("algo-esports", &["algo1", "algo", "algo esports"]),
    ("leo-team", &["leo2", "leo", "leo team"]),
    ("bad-luck", &["bad", "bad luck"]),
    ("team-oruga", &["oruga", "team oruga"]),

    // CoD Teams
    ("atlanta-faze", &["faze", "atlanta faze"]),
    ("optic-texas", &["optic", "tex", "optic texas"]),
    ("toronto-ultra", &["ultra", "toronto ultra"]),
    ("toronto-koi", &["tor", "toronto koi"]),
    ("los-angeles-thieves", &["lat", "lat1", "thieves", "la thieves"]),
    ("seattle-surge", &["surge", "seattle surge"]),
    ("vancouver-surge", &["van", "vancouver surge"]),
    ("new-york-subliners", &["nysl", "subliners", "new york subliners"]),
    ("miami-heretics", &["mh", "heretics", "miami heretics"]),
    ("las-vegas-legion", &["legion", "vegas legion"]),
    ("carolina-royal-ravens", &["crr", "ravens", "royal ravens"]),
    ("boston-breach", &["bos", "breach", "boston breach"]),
    ("los-angeles-guerrillas", &["lag", "guerrillas", "la guerrillas"]),
    ("minnesota-rokkr", &["rokkr", "minnesota rokkr"]),
    ("g2-minnesota", &["g2m", "g2 minnesota"]),
    ("faze-vegas", &["faz", "faze vegas"]),
    ("cloud9-new-york", &["c9ny", "cloud9 new york"]),
    ("riyadh-falcons", &["riyf", "riyadh falcons"]),
    ("paris-gentle-mates", &["par", "paris gentle mates"]),
];

/// Look up the canonical team name from any alias
/// Returns the canonical name if found, or None
/// Handles both space and hyphen variants (e.g., "inner-circle" matches "inner circle")
fn lookup_team_canonical(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    let normalized = lower.trim();

    // Generate both space and hyphen variants for matching
    let with_spaces = normalized.replace('-', " ");
    let with_hyphens = normalized.replace(' ', "-");

    for (canonical, aliases) in ESPORTS_TEAM_ALIASES {
        // Check canonical name (with both variants)
        let canonical_with_spaces = canonical.replace('-', " ");
        if *canonical == normalized
            || *canonical == with_spaces
            || *canonical == with_hyphens
            || canonical_with_spaces == normalized
            || canonical_with_spaces == with_spaces
        {
            return Some(canonical);
        }
        // Check all aliases (with both variants)
        for alias in *aliases {
            let alias_with_hyphens = alias.replace(' ', "-");
            let alias_with_spaces = alias.replace('-', " ");
            if *alias == normalized
                || *alias == with_spaces
                || *alias == with_hyphens
                || alias_with_hyphens == normalized
                || alias_with_hyphens == with_hyphens
                || alias_with_spaces == normalized
                || alias_with_spaces == with_spaces
            {
                return Some(canonical);
            }
        }
    }
    None
}

/// Match two team names using the canonical lookup
/// This is much more reliable than fuzzy string matching
fn teams_match_canonical(team_a: &str, team_b: &str) -> bool {
    // Try direct match first
    if team_a == team_b {
        return true;
    }

    // Look up canonical names
    let canonical_a = lookup_team_canonical(team_a);
    let canonical_b = lookup_team_canonical(team_b);

    match (canonical_a, canonical_b) {
        (Some(a), Some(b)) => a == b,
        // If one is found and matches the other's input
        (Some(a), None) => a == team_b || a.replace('-', " ") == team_b,
        (None, Some(b)) => team_a == b || team_a == b.replace('-', " "),
        // Neither found - fall back to fuzzy matching
        (None, None) => teams_match(team_a, team_b),
    }
}

// Static regex patterns compiled once for performance (avoids recompilation on each call)
static RE_ESPORTS_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*(esports|gaming|team|clan)\s*$").unwrap()
});
static RE_ESPORTS_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(team|clan)\s+").unwrap()
});
static RE_POLY_TITLE_PARENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i):\s*(.+?)\s+vs\.?\s+(.+?)\s*\(").unwrap()
});
static RE_POLY_TITLE_DASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i):\s*(.+?)\s+vs\.?\s+(.+?)\s*-").unwrap()
});
static RE_POLY_TITLE_FALLBACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(.+?)\s+vs\.?\s+(.+?)(?:\s*\(|\s*-|$)").unwrap()
});
static RE_KALSHI_TITLE_COLON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i):\s*(.+?)\s+vs\.?\s+(.+)$").unwrap()
});
static RE_KALSHI_TITLE_FALLBACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(.+?)\s+vs\.?\s+(.+)$").unwrap()
});

/// Normalize esports team name for matching
/// "FURIA Esports" -> "furia", "Cloud9 New York" -> "cloud9-new-york"
fn normalize_esports_team(name: &str) -> String {
    // Convert to lowercase first, then remove common suffixes and prefixes
    let lower = name.to_lowercase();

    // Remove common suffixes (at the end)
    let cleaned = RE_ESPORTS_SUFFIX.replace(&lower, "").to_string();

    // Remove common prefixes (at the start, like "Team Liquid" -> "Liquid")
    let cleaned = RE_ESPORTS_PREFIX.replace(&cleaned, "").to_string();

    // Also remove periods and apostrophes
    let cleaned = cleaned.replace(".", "").replace("'", "");

    // Join words with hyphens
    cleaned.split_whitespace().collect::<Vec<_>>().join("-")
}

/// Extract initials from a normalized team name
/// "ground-zero" -> "gz", "secret-whales" -> "sw"
fn extract_initials(normalized: &str) -> String {
    normalized
        .split('-')
        .filter_map(|word| word.chars().next())
        .collect()
}

/// Check if two team names match, handling abbreviations
/// "gz" matches "ground-zero", "lev" matches "leviatan", "drxc" matches "drx-challengers"
/// "wb" matches "weibo", "tes" matches "top"
fn teams_match(team_a: &str, team_b: &str) -> bool {
    // Exact match
    if team_a == team_b {
        return true;
    }
    // Prefix match (either direction)
    if team_a.starts_with(team_b) || team_b.starts_with(team_a) {
        return true;
    }
    // Initial match for hyphenated names: "gz" matches "ground-zero"
    let initials_a = extract_initials(team_a);
    let initials_b = extract_initials(team_b);
    if team_a == initials_b || team_b == initials_a {
        return true;
    }
    // Abbreviated compound match: "drxc" matches "drx-challengers"
    // Check if one starts with the first component of the other
    if let Some(first_component_a) = team_a.split('-').next() {
        if team_b.starts_with(first_component_a) && team_b.len() > first_component_a.len() {
            return true;
        }
    }
    if let Some(first_component_b) = team_b.split('-').next() {
        if team_a.starts_with(first_component_b) && team_a.len() > first_component_b.len() {
            return true;
        }
    }
    // Common esports abbreviation patterns:
    // - First letters of each word: "Top Esports" -> "top" but outcome might be "TES" (first letters)
    // - Brand abbreviations: "Weibo Gaming" -> "weibo" but outcome might be "WB"
    // Check if shorter string's chars appear at word boundaries in longer string
    let (shorter, longer) = if team_a.len() <= team_b.len() { (team_a, team_b) } else { (team_b, team_a) };
    if shorter.len() >= 2 && shorter.len() <= 4 {
        // Try matching shorter as acronym of longer (including hyphenated parts)
        let longer_parts: Vec<&str> = longer.split('-').collect();
        if longer_parts.len() >= 2 {
            // Multi-word: check if shorter matches first letters
            let first_letters: String = longer_parts.iter()
                .filter_map(|p| p.chars().next())
                .collect();
            if shorter == first_letters {
                return true;
            }
        }
        // Single word but short abbreviation: check if shorter's chars are first letters of longer's syllables
        // This handles "wb" matching "weibo" (w-eibo), "tes" matching "top" (unclear)
        // Actually, let's check if the longer word CONTAINS all chars of shorter in order
        if chars_in_order(shorter, longer) {
            return true;
        }
    }
    false
}

/// Check if all characters of `needle` appear in `haystack` in order
/// This helps match abbreviations like "wb" in "weibo" or "tes" in "topesports"
fn chars_in_order(needle: &str, haystack: &str) -> bool {
    let mut haystack_chars = haystack.chars();
    for needle_char in needle.chars() {
        loop {
            match haystack_chars.next() {
                Some(c) if c == needle_char => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// Parse Polymarket event title to extract team names
/// "Counter-Strike: Team1 vs Team2 (BO3)" -> Some((team1, team2))
fn parse_poly_event_title(title: &str) -> Option<(String, String)> {
    // Helper to extract teams from captures
    fn extract_teams(caps: &regex::Captures) -> Option<(String, String)> {
        Some((
            caps.get(1)?.as_str().trim().to_string(),
            caps.get(2)?.as_str().trim().to_string(),
        ))
    }

    // Pattern: "Game: Team1 vs Team2 (BON)"
    if let Some(caps) = RE_POLY_TITLE_PARENS.captures(title) {
        if let Some(teams) = extract_teams(&caps) {
            return Some(teams);
        }
    }

    // Fallback: "Game: Team1 vs Team2 - Tournament"
    if let Some(caps) = RE_POLY_TITLE_DASH.captures(title) {
        if let Some(teams) = extract_teams(&caps) {
            return Some(teams);
        }
    }

    // Final fallback: just "Team1 vs Team2" without colon prefix
    if let Some(caps) = RE_POLY_TITLE_FALLBACK.captures(title) {
        if let Some(teams) = extract_teams(&caps) {
            return Some(teams);
        }
    }

    None
}

/// Extract date from Polymarket slug
/// "cs2-team1-team2-2026-01-16" -> Some("2026-01-16")
fn extract_date_from_poly_slug(slug: &str) -> Option<String> {
    let parts: Vec<&str> = slug.split('-').collect();
    if parts.len() >= 4 {
        let year = parts[parts.len() - 3];
        let month = parts[parts.len() - 2];
        let day = parts[parts.len() - 1];

        if year.len() == 4 && month.len() == 2 && day.len() == 2 {
            return Some(format!("{}-{}-{}", year, month, day));
        }
    }
    None
}

/// Parse Kalshi esports event title
/// "Tournament: Team1 vs. Team2" -> Some((team1, team2))
fn parse_esports_kalshi_title(title: &str) -> Option<(String, String)> {
    // Helper to extract teams from captures
    fn extract_teams(caps: &regex::Captures) -> Option<(String, String)> {
        Some((
            caps.get(1)?.as_str().trim().to_string(),
            caps.get(2)?.as_str().trim().to_string(),
        ))
    }

    // Pattern: "Tournament: Team1 vs. Team2"
    if let Some(caps) = RE_KALSHI_TITLE_COLON.captures(title) {
        if let Some(teams) = extract_teams(&caps) {
            return Some(teams);
        }
    }

    // Fallback: just "Team1 vs Team2"
    if let Some(caps) = RE_KALSHI_TITLE_FALLBACK.captures(title) {
        if let Some(teams) = extract_teams(&caps) {
            return Some(teams);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kalshi_ticker() {
        let parsed = parse_kalshi_event_ticker("KXEPLGAME-25DEC27CFCAVL").unwrap();
        assert_eq!(parsed.date, "25DEC27");
        assert_eq!(parsed.team1, "CFC");
        assert_eq!(parsed.team2, "AVL");
    }

    #[test]
    fn test_kalshi_date_to_iso() {
        assert_eq!(kalshi_date_to_iso("25DEC27"), "2025-12-27");
        assert_eq!(kalshi_date_to_iso("25JAN01"), "2025-01-01");
    }

    #[test]
    fn test_normalize_esports_team() {
        assert_eq!(normalize_esports_team("FURIA Esports"), "furia");
        assert_eq!(normalize_esports_team("Cloud9 New York"), "cloud9-new-york");
        assert_eq!(normalize_esports_team("Team Liquid"), "liquid");
        assert_eq!(normalize_esports_team("G2"), "g2");
        assert_eq!(normalize_esports_team("BetBoom Team"), "betboom");
        assert_eq!(normalize_esports_team("Gen.G"), "geng");
    }

    #[test]
    fn test_parse_poly_event_title() {
        let (t1, t2) = parse_poly_event_title("Counter-Strike: FURIA vs 9INE (BO3)").unwrap();
        assert_eq!(t1, "FURIA");
        assert_eq!(t2, "9INE");

        let (t1, t2) = parse_poly_event_title("LoL: T1 vs DRX (BO5) - LCK Finals").unwrap();
        assert_eq!(t1, "T1");
        assert_eq!(t2, "DRX");
    }

    #[test]
    fn test_extract_date_from_poly_slug() {
        assert_eq!(
            extract_date_from_poly_slug("cs2-furia-9ine-2026-01-16"),
            Some("2026-01-16".to_string())
        );
        assert_eq!(
            extract_date_from_poly_slug("lol-t1-drx-2026-01-18"),
            Some("2026-01-18".to_string())
        );
        assert_eq!(extract_date_from_poly_slug("invalid"), None);
    }

    #[test]
    fn test_parse_esports_kalshi_title() {
        let (t1, t2) = parse_esports_kalshi_title("BLAST Bounty 2026: FURIA vs. 9INE").unwrap();
        assert_eq!(t1, "FURIA");
        assert_eq!(t2, "9INE");
    }

    #[test]
    fn test_filter_pairs_by_leagues_nba() {
        let pairs = vec![
            MarketPair {
                pair_id: "p1".into(),
                league: "nba".into(),
                market_type: MarketType::Moneyline,
                description: "NBA".into(),
                kalshi_event_ticker: "KXNBA-1".into(),
                kalshi_market_ticker: "KXNBA-1-A".into(),
                kalshi_event_slug: "nba-game".into(),
                poly_slug: "nba-market".into(),
                poly_yes_token: "yes".into(),
                poly_no_token: "no".into(),
                line_value: None,
                team_suffix: None,
            },
            MarketPair {
                pair_id: "p2".into(),
                league: "epl".into(),
                market_type: MarketType::Moneyline,
                description: "EPL".into(),
                kalshi_event_ticker: "KXEPL-1".into(),
                kalshi_market_ticker: "KXEPL-1-A".into(),
                kalshi_event_slug: "premier-league-game".into(),
                poly_slug: "epl-market".into(),
                poly_yes_token: "yes".into(),
                poly_no_token: "no".into(),
                line_value: None,
                team_suffix: None,
            },
        ];

        let filtered = filter_pairs_by_leagues(pairs, &["nba"]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(&*filtered[0].league, "nba");
    }

    /// Helper to simulate the token mapping logic from discovery
    /// Returns (team1_token, team2_token, poly_team1_norm)
    fn map_tokens_to_teams(
        title_team1: &str,
        title_team2: &str,
        outcomes: &[&str],
        token_ids: &[&str],
    ) -> (String, String, String) {
        let norm1 = normalize_esports_team(title_team1);
        let norm2 = normalize_esports_team(title_team2);
        let outcome0_norm = normalize_esports_team(outcomes[0]);
        let outcome1_norm = normalize_esports_team(outcomes[1]);

        if outcome0_norm == norm1 || outcome1_norm == norm2 {
            // outcomes[0] is team1, outcomes[1] is team2
            (token_ids[0].to_string(), token_ids[1].to_string(), outcome0_norm)
        } else if outcome1_norm == norm1 || outcome0_norm == norm2 {
            // outcomes[1] is team1, outcomes[0] is team2
            (token_ids[1].to_string(), token_ids[0].to_string(), outcome1_norm)
        } else {
            // Fallback: use token order as-is
            (token_ids[0].to_string(), token_ids[1].to_string(), outcome0_norm)
        }
    }

    /// Helper to simulate the token swap logic when matching Kalshi to Poly
    /// Returns (poly_yes_token, poly_no_token)
    fn assign_poly_tokens(
        team1_token: &str,
        team2_token: &str,
        poly_team1_norm: &str,
        kalshi_team: &str,
    ) -> (String, String) {
        let kalshi_team_norm = normalize_esports_team(kalshi_team);
        if kalshi_team_norm == poly_team1_norm {
            // Kalshi asks about team1 → use tokens as-is
            (team1_token.to_string(), team2_token.to_string())
        } else {
            // Kalshi asks about team2 → swap tokens
            (team2_token.to_string(), team1_token.to_string())
        }
    }

    #[test]
    fn test_token_mapping_outcomes_match_title_order() {
        // Title: "FURIA vs 9INE", Outcomes: ["FURIA", "9INE"]
        // Tokens should map directly: FURIA=token0, 9INE=token1
        let (t1_token, t2_token, poly_team1) = map_tokens_to_teams(
            "FURIA",
            "9INE",
            &["FURIA Esports", "9INE Gaming"],
            &["FURIA_TOKEN", "9INE_TOKEN"],
        );
        assert_eq!(t1_token, "FURIA_TOKEN");
        assert_eq!(t2_token, "9INE_TOKEN");
        assert_eq!(poly_team1, "furia");
    }

    #[test]
    fn test_token_mapping_outcomes_reversed_from_title() {
        // Title: "FURIA vs 9INE", Outcomes: ["9INE", "FURIA"]
        // Tokens should be swapped: FURIA=token1, 9INE=token0
        let (t1_token, t2_token, poly_team1) = map_tokens_to_teams(
            "FURIA",
            "9INE",
            &["9INE Gaming", "FURIA Esports"],
            &["9INE_TOKEN", "FURIA_TOKEN"],
        );
        assert_eq!(t1_token, "FURIA_TOKEN", "FURIA should get token1");
        assert_eq!(t2_token, "9INE_TOKEN", "9INE should get token0");
        assert_eq!(poly_team1, "furia");
    }

    #[test]
    fn test_token_mapping_red_canids_vs_leviatan_bug() {
        // This is the actual bug case from production:
        // Poly title: "Leviatan Esports vs RED Canids"
        // Poly outcomes: ["Leviatan Esports", "RED Canids"]
        // Poly tokens: [LEV_TOKEN, RED_TOKEN]
        // Expected: Leviatan=LEV_TOKEN, RED=RED_TOKEN
        let (t1_token, t2_token, poly_team1) = map_tokens_to_teams(
            "Leviatan Esports",
            "RED Canids",
            &["Leviatan Esports", "RED Canids"],
            &["LEV_TOKEN", "RED_TOKEN"],
        );
        assert_eq!(t1_token, "LEV_TOKEN", "Leviatan should get LEV_TOKEN");
        assert_eq!(t2_token, "RED_TOKEN", "RED should get RED_TOKEN");
        assert_eq!(poly_team1, "leviatan");
    }

    #[test]
    fn test_token_mapping_red_canids_vs_leviatan_reversed_outcomes() {
        // Same match but outcomes array is reversed
        // Poly title: "Leviatan Esports vs RED Canids"
        // Poly outcomes: ["RED Canids", "Leviatan Esports"]
        // Poly tokens: [RED_TOKEN, LEV_TOKEN]
        // Expected: Leviatan=LEV_TOKEN (token1), RED=RED_TOKEN (token0)
        let (t1_token, t2_token, poly_team1) = map_tokens_to_teams(
            "Leviatan Esports",
            "RED Canids",
            &["RED Canids", "Leviatan Esports"],
            &["RED_TOKEN", "LEV_TOKEN"],
        );
        assert_eq!(t1_token, "LEV_TOKEN", "Leviatan should get LEV_TOKEN");
        assert_eq!(t2_token, "RED_TOKEN", "RED should get RED_TOKEN");
        assert_eq!(poly_team1, "leviatan");
    }

    #[test]
    fn test_kalshi_poly_token_assignment_kalshi_asks_team1() {
        // Kalshi market: "Will Leviatan win?"
        // poly_team1 = "leviatan" (Leviatan is team1 in poly_lookup)
        // team1_token = LEV_TOKEN, team2_token = RED_TOKEN
        // Since Kalshi asks about Leviatan (team1), use tokens as-is
        let (poly_yes, poly_no) = assign_poly_tokens(
            "LEV_TOKEN",
            "RED_TOKEN",
            "leviatan",
            "Leviatan Esports",
        );
        assert_eq!(poly_yes, "LEV_TOKEN", "Kalshi YES (Leviatan wins) = Poly LEV");
        assert_eq!(poly_no, "RED_TOKEN", "Kalshi NO (Leviatan loses) = Poly RED");
    }

    #[test]
    fn test_kalshi_poly_token_assignment_kalshi_asks_team2() {
        // Kalshi market: "Will RED Canids win?"
        // poly_team1 = "leviatan" (Leviatan is team1 in poly_lookup)
        // team1_token = LEV_TOKEN, team2_token = RED_TOKEN
        // Since Kalshi asks about RED (team2), swap tokens
        let (poly_yes, poly_no) = assign_poly_tokens(
            "LEV_TOKEN",
            "RED_TOKEN",
            "leviatan",
            "RED Canids",
        );
        assert_eq!(poly_yes, "RED_TOKEN", "Kalshi YES (RED wins) = Poly RED");
        assert_eq!(poly_no, "LEV_TOKEN", "Kalshi NO (RED loses) = Poly LEV");
    }

    #[test]
    fn test_full_token_flow_leviatan_market() {
        // Full flow test for the bug case:
        // Poly: "Leviatan Esports vs RED Canids", outcomes=["Leviatan Esports", "RED Canids"]
        // Kalshi: "Will Leviatan Esports win?" (asking about Leviatan)
        // Prices: Kalshi Leviatan YES=16¢, Poly LEV=22¢, Poly RED=91¢

        // Step 1: Map tokens from Poly data
        let (team1_token, team2_token, poly_team1) = map_tokens_to_teams(
            "Leviatan Esports",
            "RED Canids",
            &["Leviatan Esports", "RED Canids"],
            &["LEV_22", "RED_91"],  // Using prices as token names for clarity
        );

        // Step 2: Assign poly_yes/poly_no based on Kalshi market
        let (poly_yes, poly_no) = assign_poly_tokens(
            &team1_token,
            &team2_token,
            &poly_team1,
            "Leviatan Esports",  // Kalshi asks about Leviatan
        );

        // Verify correct assignment:
        // Kalshi YES = Leviatan wins → should pair with LEV token (22¢)
        // Kalshi NO = Leviatan loses = RED wins → should pair with RED token (91¢)
        assert_eq!(poly_yes, "LEV_22", "Kalshi YES should pair with Poly LEV (22¢)");
        assert_eq!(poly_no, "RED_91", "Kalshi NO should pair with Poly RED (91¢)");

        // The bug was: poly_no was incorrectly set to LEV (22¢) instead of RED (91¢)
        // This caused false arbitrage detection: 16¢ + 22¢ = 38¢ (fake arb!)
        // Correct: 16¢ + 91¢ = 107¢ (no arb)
    }

    #[test]
    fn test_full_token_flow_red_canids_market() {
        // Same match, but Kalshi asks about RED Canids instead
        // Kalshi: "Will RED Canids win?"

        let (team1_token, team2_token, poly_team1) = map_tokens_to_teams(
            "Leviatan Esports",
            "RED Canids",
            &["Leviatan Esports", "RED Canids"],
            &["LEV_22", "RED_91"],
        );

        let (poly_yes, poly_no) = assign_poly_tokens(
            &team1_token,
            &team2_token,
            &poly_team1,
            "RED Canids",  // Kalshi asks about RED
        );

        // Kalshi YES = RED wins → should pair with RED token (91¢)
        // Kalshi NO = RED loses = Leviatan wins → should pair with LEV token (22¢)
        assert_eq!(poly_yes, "RED_91", "Kalshi YES (RED wins) = Poly RED");
        assert_eq!(poly_no, "LEV_22", "Kalshi NO (RED loses) = Poly LEV");
    }

    #[test]
    fn test_normalize_handles_esports_suffixes() {
        // Verify normalization handles common esports team name variations
        assert_eq!(normalize_esports_team("RED Canids"), "red-canids");
        assert_eq!(normalize_esports_team("Leviatan Esports"), "leviatan");
        assert_eq!(normalize_esports_team("LOUD"), "loud");
        assert_eq!(normalize_esports_team("paiN Gaming"), "pain");
    }

    #[test]
    fn test_extract_initials() {
        assert_eq!(extract_initials("ground-zero"), "gz");
        assert_eq!(extract_initials("secret-whales"), "sw");
        assert_eq!(extract_initials("leviatan"), "l");
        assert_eq!(extract_initials("red-canids"), "rc");
        assert_eq!(extract_initials("furia"), "f");
    }

    #[test]
    fn test_teams_match_exact() {
        assert!(teams_match("furia", "furia"));
        assert!(teams_match("leviatan", "leviatan"));
    }

    #[test]
    fn test_teams_match_prefix() {
        // "lev" is prefix of "leviatan"
        assert!(teams_match("lev", "leviatan"));
        // "red" is prefix of "red-canids"
        assert!(teams_match("red", "red-canids"));
    }

    #[test]
    fn test_teams_match_initials() {
        // "gz" matches "ground-zero" via initials
        assert!(teams_match("gz", "ground-zero"));
        // "sw" matches "secret-whales" via initials
        assert!(teams_match("sw", "secret-whales"));
        // "rc" matches "red-canids" via initials
        assert!(teams_match("rc", "red-canids"));
    }

    #[test]
    fn test_teams_match_no_match() {
        // Different teams should not match
        assert!(!teams_match("furia", "leviatan"));
        assert!(!teams_match("gz", "secret-whales"));
        assert!(!teams_match("lev", "red-canids"));
    }

    #[test]
    fn test_teams_match_ground_zero_bug() {
        // This was the actual bug: "gz" should match "ground-zero"
        let kalshi_team = normalize_esports_team("GZ");
        let poly_team = normalize_esports_team("Ground Zero Gaming");
        assert_eq!(kalshi_team, "gz");
        assert_eq!(poly_team, "ground-zero");
        assert!(teams_match(&kalshi_team, &poly_team), "GZ should match Ground Zero Gaming");
    }

    #[test]
    fn test_teams_match_drx_challengers() {
        // DRX should match "drx-challengers" via prefix
        let kalshi_team = normalize_esports_team("DRX");
        let poly_team = normalize_esports_team("DRX Challengers");
        assert_eq!(kalshi_team, "drx");
        assert_eq!(poly_team, "drx-challengers");
        assert!(teams_match(&kalshi_team, &poly_team), "DRX should match DRX Challengers");
    }

    #[test]
    fn test_teams_match_kt_rolster_challengers() {
        // KT or KTC should match "kt-rolster-challengers"
        let kalshi_team = normalize_esports_team("KT");
        let poly_team = normalize_esports_team("KT Rolster Challengers");
        assert_eq!(kalshi_team, "kt");
        assert_eq!(poly_team, "kt-rolster-challengers");
        assert!(teams_match(&kalshi_team, &poly_team), "KT should match KT Rolster Challengers");
    }

    #[test]
    fn test_teams_match_drxc_compound_abbreviation() {
        // DRXC should match "drx-challengers" via compound abbreviation
        // "drxc" starts with "drx" (first component) and is longer
        let kalshi_team = normalize_esports_team("DRXC");
        let poly_team = normalize_esports_team("DRX Challengers");
        assert_eq!(kalshi_team, "drxc");
        assert_eq!(poly_team, "drx-challengers");
        assert!(teams_match(&kalshi_team, &poly_team), "DRXC should match DRX Challengers");
    }

    #[test]
    fn test_teams_match_ktc_compound_abbreviation() {
        // KTC should match "kt-rolster-challengers" via compound abbreviation
        // "ktc" starts with "kt" (first component) and is longer
        let kalshi_team = normalize_esports_team("KTC");
        let poly_team = normalize_esports_team("KT Rolster Challengers");
        assert_eq!(kalshi_team, "ktc");
        assert_eq!(poly_team, "kt-rolster-challengers");
        assert!(teams_match(&kalshi_team, &poly_team), "KTC should match KT Rolster Challengers");
    }

    #[test]
    fn test_teams_match_weibo_wb() {
        // WB should match "weibo" via prefix
        let outcome_norm = normalize_esports_team("WB");
        let title_norm = normalize_esports_team("Weibo Gaming");
        assert_eq!(outcome_norm, "wb");
        assert_eq!(title_norm, "weibo");
        assert!(teams_match(&outcome_norm, &title_norm), "WB should match Weibo Gaming");
    }

    #[test]
    fn test_teams_match_canonical_tes_top_esports() {
        // TES should match "Top Esports" via canonical lookup
        assert!(teams_match_canonical("TES", "Top Esports"),
                "TES should match Top Esports via canonical lookup");
        assert!(teams_match_canonical("tes", "top-esports"),
                "tes should match top-esports via canonical lookup");
    }

    #[test]
    fn test_teams_match_canonical_wb_weibo() {
        // WB should match "Weibo Gaming" via canonical lookup
        assert!(teams_match_canonical("WB", "Weibo Gaming"),
                "WB should match Weibo Gaming via canonical lookup");
        assert!(teams_match_canonical("wb", "weibo-gaming"),
                "wb should match weibo-gaming via canonical lookup");
    }

    #[test]
    fn test_weibo_vs_tes_token_mapping_canonical() {
        // This is the actual bug case:
        // Poly title: "Weibo Gaming vs Top Esports"
        // Poly outcomes: ["TES", "WB"]
        // Poly tokens: [TES_TOKEN, WB_TOKEN]
        //
        // Using canonical lookup:
        // - outcome "TES" matches "Top Esports"? YES
        // - outcome "WB" matches "Weibo Gaming"? YES
        // - outcome "TES" matches "Weibo Gaming"? NO
        // - outcome "WB" matches "Top Esports"? NO

        let team1 = "Weibo Gaming";
        let team2 = "Top Esports";
        let outcomes = ["TES", "WB"];

        // Check what should match using canonical lookup (same as discovery code)
        let outcome0_matches_team1 = teams_match_canonical(&outcomes[0], team1); // TES vs Weibo = false
        let outcome1_matches_team1 = teams_match_canonical(&outcomes[1], team1); // WB vs Weibo = true
        let outcome0_matches_team2 = teams_match_canonical(&outcomes[0], team2); // TES vs Top Esports = true
        let outcome1_matches_team2 = teams_match_canonical(&outcomes[1], team2); // WB vs Top Esports = false

        assert!(!outcome0_matches_team1, "TES should NOT match Weibo Gaming");
        assert!(outcome1_matches_team1, "WB should match Weibo Gaming");
        assert!(outcome0_matches_team2, "TES should match Top Esports");
        assert!(!outcome1_matches_team2, "WB should NOT match Top Esports");

        // Determine token assignment using the same logic as discovery code
        let (team1_token, team2_token) =
            if outcome0_matches_team1 || outcome1_matches_team2 {
                // outcomes[0] is team1, outcomes[1] is team2
                ("TES_TOKEN", "WB_TOKEN")
            } else if outcome1_matches_team1 || outcome0_matches_team2 {
                // outcomes[1] is team1, outcomes[0] is team2
                ("WB_TOKEN", "TES_TOKEN")
            } else {
                panic!("Should have matched");
            };

        // team1 = Weibo, team2 = Top Esports
        // team1_token should be WB_TOKEN (Weibo's token)
        // team2_token should be TES_TOKEN (Top Esports' token)
        assert_eq!(team1_token, "WB_TOKEN", "Weibo should get WB token");
        assert_eq!(team2_token, "TES_TOKEN", "Top Esports should get TES token");
    }

    #[test]
    fn test_lookup_team_canonical() {
        // Test the canonical lookup function
        assert_eq!(lookup_team_canonical("TES"), Some("top-esports"));
        assert_eq!(lookup_team_canonical("tes"), Some("top-esports"));
        assert_eq!(lookup_team_canonical("Top Esports"), Some("top-esports"));
        assert_eq!(lookup_team_canonical("WB"), Some("weibo-gaming"));
        assert_eq!(lookup_team_canonical("wb"), Some("weibo-gaming"));
        assert_eq!(lookup_team_canonical("Weibo Gaming"), Some("weibo-gaming"));
        assert_eq!(lookup_team_canonical("G2"), Some("g2-esports"));
        assert_eq!(lookup_team_canonical("T1"), Some("t1"));
        assert_eq!(lookup_team_canonical("unknown_team"), None);
    }

    #[test]
    fn test_gentle_mates_m8_canonical() {
        // Test Gentle Mates (M8) alias lookup
        assert_eq!(lookup_team_canonical("M8"), Some("gentle-mates"));
        assert_eq!(lookup_team_canonical("m8"), Some("gentle-mates"));
        assert_eq!(lookup_team_canonical("M 8"), Some("gentle-mates"));
        assert_eq!(lookup_team_canonical("m-8"), Some("gentle-mates"));
        assert_eq!(lookup_team_canonical("Gentle Mates"), Some("gentle-mates"));
        assert_eq!(lookup_team_canonical("gentle mates"), Some("gentle-mates"));
    }

    #[test]
    fn test_team_liquid_tl1_canonical() {
        // Test Team Liquid with numbered suffix (TL1) used by Polymarket
        assert_eq!(lookup_team_canonical("TL"), Some("team-liquid"));
        assert_eq!(lookup_team_canonical("TL1"), Some("team-liquid"));
        assert_eq!(lookup_team_canonical("tl1"), Some("team-liquid"));
        assert_eq!(lookup_team_canonical("TL 1"), Some("team-liquid"));
        assert_eq!(lookup_team_canonical("tl-1"), Some("team-liquid"));
        assert_eq!(lookup_team_canonical("Liquid"), Some("team-liquid"));
        assert_eq!(lookup_team_canonical("Team Liquid"), Some("team-liquid"));
    }

    #[test]
    fn test_gentle_mates_vs_liquid_matching() {
        // This is the actual bug case:
        // Kalshi: "Liquid vs Gentle Mates" with markets for TL and M8
        // Polymarket: "M 8 vs TL 1" with outcomes ["M 8", "TL 1"]

        // Verify M8 matches Gentle Mates
        assert!(teams_match_canonical("M8", "Gentle Mates"),
                "M8 should match Gentle Mates");
        assert!(teams_match_canonical("M 8", "gentle-mates"),
                "M 8 should match gentle-mates");

        // Verify TL1 matches Liquid/Team Liquid
        assert!(teams_match_canonical("TL1", "Liquid"),
                "TL1 should match Liquid");
        assert!(teams_match_canonical("TL 1", "Team Liquid"),
                "TL 1 should match Team Liquid");
        assert!(teams_match_canonical("tl-1", "liquid"),
                "tl-1 should match liquid");

        // Verify they don't cross-match
        assert!(!teams_match_canonical("M8", "Liquid"),
                "M8 should NOT match Liquid");
        assert!(!teams_match_canonical("TL1", "Gentle Mates"),
                "TL1 should NOT match Gentle Mates");
    }

    #[test]
    fn test_space_hyphen_variant_matching() {
        // This tests the fix for the Inner Circle bug where:
        // - normalize_esports_team("Inner Circle Esports") = "inner-circle" (hyphens)
        // - alias in map is "inner circle" (spaces)
        // The lookup should match both variants

        // Inner Circle with hyphens should find the alias with spaces
        assert_eq!(lookup_team_canonical("inner-circle"), Some("inner-circle-esports"),
                   "inner-circle (hyphen) should match inner circle (space) alias");

        // ICE should still work
        assert_eq!(lookup_team_canonical("ICE"), Some("inner-circle-esports"),
                   "ICE should match inner-circle-esports");

        // Full matching should work
        assert!(teams_match_canonical("ice", "inner-circle"),
                "ice should match inner-circle via canonical lookup");
        assert!(teams_match_canonical("ICE", "Inner Circle Esports"),
                "ICE should match Inner Circle Esports");

        // Test other teams with spaces in their names
        assert_eq!(lookup_team_canonical("los-ratones"), Some("los-ratones"),
                   "los-ratones should match");
        assert_eq!(lookup_team_canonical("los ratones"), Some("los-ratones"),
                   "los ratones (spaces) should match los-ratones");
    }

    #[test]
    fn test_market_type_filter_none_includes_all() {
        // When filter is None, all market types should be included
        let all_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
        let filter: Option<MarketType> = None;

        let filtered: Vec<_> = all_types.iter()
            .filter(|mt| filter.map_or(true, |f| f == **mt))
            .copied()
            .collect();

        assert_eq!(filtered.len(), 4);
        assert!(filtered.contains(&MarketType::Moneyline));
        assert!(filtered.contains(&MarketType::Spread));
        assert!(filtered.contains(&MarketType::Total));
        assert!(filtered.contains(&MarketType::Btts));
    }

    #[test]
    fn test_market_type_filter_moneyline_only() {
        // When filter is Moneyline, only Moneyline should be included
        let all_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
        let filter = Some(MarketType::Moneyline);

        let filtered: Vec<_> = all_types.iter()
            .filter(|mt| filter.map_or(true, |f| f == **mt))
            .copied()
            .collect();

        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains(&MarketType::Moneyline));
        assert!(!filtered.contains(&MarketType::Spread));
    }

    #[test]
    fn test_market_type_filter_spread_only() {
        // When filter is Spread, only Spread should be included
        let all_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
        let filter = Some(MarketType::Spread);

        let filtered: Vec<_> = all_types.iter()
            .filter(|mt| filter.map_or(true, |f| f == **mt))
            .copied()
            .collect();

        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains(&MarketType::Spread));
        assert!(!filtered.contains(&MarketType::Moneyline));
    }

    #[test]
    fn test_market_type_filter_total_only() {
        // When filter is Total, only Total should be included
        let all_types = [MarketType::Moneyline, MarketType::Spread, MarketType::Total, MarketType::Btts];
        let filter = Some(MarketType::Total);

        let filtered: Vec<_> = all_types.iter()
            .filter(|mt| filter.map_or(true, |f| f == **mt))
            .copied()
            .collect();

        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains(&MarketType::Total));
    }

    // ========================================================================
    // SPORTS TOKEN ASSIGNMENT TESTS
    // These tests verify the fix for the numeric suffix bug in spread markets
    // ========================================================================

    /// Helper to simulate the sports token assignment logic from discovery
    /// This mirrors the logic at lines 539-600 of discovery.rs
    fn assign_sports_tokens(
        suffix: &str,
        outcomes: &[&str],
        token1: &str,
        token2: &str,
    ) -> (String, String, &'static str) {
        // Strip numeric suffix (e.g., "LEE1" -> "LEE", "ATL" -> "ATL")
        let team_code: String = suffix.chars()
            .take_while(|c| c.is_alphabetic())
            .collect::<String>()
            .to_lowercase();

        let outcome0_matches = outcomes.get(0)
            .map(|o| o.to_lowercase().contains(&team_code))
            .unwrap_or(false);
        let outcome1_matches = outcomes.get(1)
            .map(|o| o.to_lowercase().contains(&team_code))
            .unwrap_or(false);

        if outcome0_matches && !outcome1_matches {
            (token1.to_string(), token2.to_string(), "token1=YES (outcome[0] matched)")
        } else if outcome1_matches && !outcome0_matches {
            (token2.to_string(), token1.to_string(), "token2=YES (outcome[1] matched)")
        } else {
            (token1.to_string(), token2.to_string(), "API order (ambiguous/no match)")
        }
    }

    // --- NBA Moneyline Tests (Real Production Examples) ---

    #[test]
    fn test_nba_moneyline_atl_suffix() {
        // Kalshi: KXNBAGAME-26JAN17BOSATL-ATL (Boston at Atlanta, betting on Atlanta)
        // Polymarket outcomes: ["Boston Celtics", "Atlanta Hawks"]
        let (yes, no, reason) = assign_sports_tokens(
            "ATL",
            &["Boston Celtics", "Atlanta Hawks"],
            "BOS_TOKEN",
            "ATL_TOKEN",
        );
        assert_eq!(yes, "ATL_TOKEN", "ATL should match 'Atlanta Hawks' in outcome[1]");
        assert_eq!(no, "BOS_TOKEN", "NO should be Boston token");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    #[test]
    fn test_nba_moneyline_bos_suffix() {
        // Kalshi: KXNBAGAME-26JAN17BOSATL-BOS (Boston at Atlanta, betting on Boston)
        // Polymarket outcomes: ["Boston Celtics", "Atlanta Hawks"]
        let (yes, no, reason) = assign_sports_tokens(
            "BOS",
            &["Boston Celtics", "Atlanta Hawks"],
            "BOS_TOKEN",
            "ATL_TOKEN",
        );
        assert_eq!(yes, "BOS_TOKEN", "BOS should match 'Boston Celtics' in outcome[0]");
        assert_eq!(no, "ATL_TOKEN", "NO should be Atlanta token");
        assert_eq!(reason, "token1=YES (outcome[0] matched)");
    }

    // --- NHL Moneyline Tests ---

    #[test]
    fn test_nhl_moneyline_van_suffix() {
        // Kalshi: KXNHLGAME-26JAN17EDMVAN-VAN (Edmonton at Vancouver, betting on Vancouver)
        // Polymarket outcomes: ["Edmonton Oilers", "Vancouver Canucks"]
        let (yes, no, reason) = assign_sports_tokens(
            "VAN",
            &["Edmonton Oilers", "Vancouver Canucks"],
            "EDM_TOKEN",
            "VAN_TOKEN",
        );
        assert_eq!(yes, "VAN_TOKEN", "VAN should match 'Vancouver Canucks' in outcome[1]");
        assert_eq!(no, "EDM_TOKEN", "NO should be Edmonton token");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    // --- EPL Spread Tests (The Bug Case) ---

    #[test]
    fn test_epl_spread_lee1_suffix_before_fix() {
        // This was the BUG: suffix "LEE1" doesn't match "Leeds United FC"
        // because we were checking if "leeds united fc".contains("lee1")
        // Now we strip the numeric: "LEE1" -> "lee" and check contains("lee")
        let (yes, no, reason) = assign_sports_tokens(
            "LEE1",  // Spread suffix includes the spread value
            &["Everton FC", "Leeds United FC"],
            "EVE_TOKEN",
            "LEE_TOKEN",
        );
        assert_eq!(yes, "LEE_TOKEN", "LEE1 should strip to 'lee' and match 'Leeds United FC'");
        assert_eq!(no, "EVE_TOKEN", "NO should be Everton token");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    #[test]
    fn test_epl_spread_mun1_suffix_no_match() {
        // Arsenal vs Manchester United, betting on Man United spread
        // NOTE: "mun" is NOT a substring of "manchester united fc"
        // (MUN is an abbreviation, not a substring)
        // This falls back to API order - which may be wrong for some markets
        let (yes, no, reason) = assign_sports_tokens(
            "MUN1",
            &["Arsenal FC", "Manchester United FC"],
            "ARS_TOKEN",
            "MUN_TOKEN",
        );
        // Neither outcome contains "mun", so API order is used
        assert_eq!(reason, "API order (ambiguous/no match)");
    }

    #[test]
    fn test_epl_spread_man1_suffix_matches() {
        // If Kalshi used "MAN1" instead of "MUN1", it would match
        let (yes, no, reason) = assign_sports_tokens(
            "MAN1",
            &["Arsenal FC", "Manchester United FC"],
            "ARS_TOKEN",
            "MAN_TOKEN",
        );
        assert_eq!(yes, "MAN_TOKEN", "MAN1 should strip to 'man' and match 'Manchester United FC'");
        assert_eq!(no, "ARS_TOKEN");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    #[test]
    fn test_epl_spread_bou1_suffix() {
        // Liverpool vs Bournemouth, betting on Bournemouth spread
        // Note: outcomes may have different order
        let (yes, no, reason) = assign_sports_tokens(
            "BOU1",
            &["Liverpool FC", "AFC Bournemouth"],
            "LIV_TOKEN",
            "BOU_TOKEN",
        );
        assert_eq!(yes, "BOU_TOKEN", "BOU1 should strip to 'bou' and match 'AFC Bournemouth'");
        assert_eq!(no, "LIV_TOKEN");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    #[test]
    fn test_epl_spread_cry1_away_favored() {
        // Chelsea vs Crystal Palace, betting on Crystal Palace (away favored)
        // Outcomes: ["Chelsea FC", "Crystal Palace FC"]
        let (yes, no, reason) = assign_sports_tokens(
            "CRY1",
            &["Chelsea FC", "Crystal Palace FC"],
            "CHE_TOKEN",
            "CRY_TOKEN",
        );
        assert_eq!(yes, "CRY_TOKEN", "CRY1 should match 'Crystal Palace FC'");
        assert_eq!(no, "CHE_TOKEN");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    // --- BTTS/Total Tests (Yes/No outcomes) ---

    #[test]
    fn test_btts_yes_no_outcomes() {
        // BTTS markets have Yes/No outcomes, no team matching possible
        // Should fall back to API order which is [Yes, No]
        let (yes, no, reason) = assign_sports_tokens(
            "ARSMUN",  // BTTS suffix is typically the event, no team suffix
            &["Yes", "No"],
            "YES_TOKEN",
            "NO_TOKEN",
        );
        // Neither outcome contains "arsmun", so API order is used
        assert_eq!(yes, "YES_TOKEN");
        assert_eq!(no, "NO_TOKEN");
        assert_eq!(reason, "API order (ambiguous/no match)");
    }

    // --- Edge Cases ---

    #[test]
    fn test_suffix_with_large_spread_value() {
        // Spread suffix with larger value like "WOL2" (2.5 goals)
        let (yes, no, reason) = assign_sports_tokens(
            "WOL2",
            &["Manchester City FC", "Wolverhampton Wanderers FC"],
            "MCI_TOKEN",
            "WOL_TOKEN",
        );
        assert_eq!(yes, "WOL_TOKEN", "WOL2 should strip to 'wol' and match 'Wolverhampton'");
        assert_eq!(no, "MCI_TOKEN");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    #[test]
    fn test_suffix_matching_first_outcome() {
        // When the team code matches the FIRST outcome
        let (yes, no, reason) = assign_sports_tokens(
            "ARS1",
            &["Arsenal FC", "Manchester United FC"],
            "ARS_TOKEN",
            "MUN_TOKEN",
        );
        assert_eq!(yes, "ARS_TOKEN", "ARS1 should match 'Arsenal FC' in outcome[0]");
        assert_eq!(no, "MUN_TOKEN");
        assert_eq!(reason, "token1=YES (outcome[0] matched)");
    }

    #[test]
    fn test_both_outcomes_contain_suffix() {
        // Edge case: suffix matches BOTH outcomes (e.g., "FC" in both)
        // This should fall back to API order
        let (yes, no, reason) = assign_sports_tokens(
            "FC",
            &["Arsenal FC", "Manchester United FC"],
            "ARS_TOKEN",
            "MUN_TOKEN",
        );
        // Both contain "fc", so ambiguous
        assert_eq!(reason, "API order (ambiguous/no match)");
    }

    #[test]
    fn test_neither_outcome_matches() {
        // Edge case: suffix matches neither outcome
        let (yes, no, reason) = assign_sports_tokens(
            "XYZ",
            &["Arsenal FC", "Manchester United FC"],
            "ARS_TOKEN",
            "MUN_TOKEN",
        );
        assert_eq!(reason, "API order (ambiguous/no match)");
    }

    // --- Real Bug Reproduction Tests ---

    #[test]
    fn test_bur1_burnley_spread_bug() {
        // This was a production bug:
        // Kalshi suffix: BUR1 (Burnley +1.5)
        // Poly outcomes: ["Tottenham Hotspur FC", "Burnley FC"]
        // Bug: "bur1" doesn't contain in "burnley fc"
        // Fix: Strip to "bur", which IS in "burnley fc"
        let (yes, no, reason) = assign_sports_tokens(
            "BUR1",
            &["Tottenham Hotspur FC", "Burnley FC"],
            "TOT_TOKEN",
            "BUR_TOKEN",
        );
        assert_eq!(yes, "BUR_TOKEN", "BUR1 should strip to 'bur' and match 'Burnley FC'");
        assert_eq!(no, "TOT_TOKEN");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    #[test]
    fn test_nfo1_nottingham_spread_no_match() {
        // Kalshi suffix: NFO1 (Nottingham Forest +1.5)
        // Poly outcomes: ["Brentford FC", "Nottingham Forest FC"]
        // NOTE: "nfo" is NOT a substring of "nottingham forest fc"
        // NFO is an abbreviation (Nottingham FOrest), not a substring
        let (yes, no, reason) = assign_sports_tokens(
            "NFO1",
            &["Brentford FC", "Nottingham Forest FC"],
            "BRE_TOKEN",
            "NFO_TOKEN",
        );
        // Neither outcome contains "nfo", falls back to API order
        assert_eq!(reason, "API order (ambiguous/no match)");
    }

    #[test]
    fn test_not1_nottingham_spread_matches() {
        // If Kalshi used "NOT1" instead, it would match "Nottingham"
        let (yes, no, reason) = assign_sports_tokens(
            "NOT1",
            &["Brentford FC", "Nottingham Forest FC"],
            "BRE_TOKEN",
            "NOT_TOKEN",
        );
        assert_eq!(yes, "NOT_TOKEN", "NOT1 should strip to 'not' and match 'Nottingham Forest FC'");
        assert_eq!(no, "BRE_TOKEN");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }

    #[test]
    fn test_sun1_sunderland_spread_bug() {
        // Kalshi suffix: SUN1 (Sunderland +1.5)
        // Poly outcomes: ["West Ham United FC", "Sunderland AFC"]
        let (yes, no, reason) = assign_sports_tokens(
            "SUN1",
            &["West Ham United FC", "Sunderland AFC"],
            "WHU_TOKEN",
            "SUN_TOKEN",
        );
        // "sun" is in "sunderland afc"
        assert_eq!(yes, "SUN_TOKEN", "SUN1 should strip to 'sun' and match 'Sunderland AFC'");
        assert_eq!(no, "WHU_TOKEN");
        assert_eq!(reason, "token2=YES (outcome[1] matched)");
    }
}
