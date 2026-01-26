//! CLI tool to generate NCAA Men's Basketball team code mappings.
//!
//! Usage: cargo run --bin generate-ncaamb-cache
//!
//! This tool:
//! 1. Fetches all Kalshi NCAAMB events with team codes and names
//! 2. Probes Polymarket with multiple slug patterns to find matches
//! 3. Extracts non-identity mappings (where Kalshi code != Poly code)
//! 4. Saves mappings to kalshi_team_cache.json

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const KALSHI_API_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

#[derive(Debug, Deserialize)]
struct KalshiMarketsResponse {
    markets: Vec<KalshiMarket>,
}

#[derive(Debug, Deserialize)]
struct KalshiMarket {
    event_ticker: String,
    ticker: String,
    title: String,
    yes_sub_title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PolyEvent {
    slug: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct TeamCacheFile {
    mappings: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct UnmatchedTeam {
    kalshi_code: String,
    team_name: String,
    event_ticker: String,
    date: String,
}

/// Extract team suffix from ticker (e.g., "KXNCAAMBGAME-26JAN24ILLPUR-ILL" -> "ILL")
fn extract_suffix(ticker: &str) -> Option<String> {
    ticker.split('-').last().map(|s| s.to_uppercase())
}

/// Parse date from event ticker (e.g., "KXNCAAMBGAME-26JAN24ILLPUR" -> "2026-01-24")
fn parse_date(event_ticker: &str) -> Option<String> {
    let parts: Vec<&str> = event_ticker.split('-').collect();
    if parts.len() < 2 {
        return None;
    }

    let date_teams = parts[1];
    if date_teams.len() < 7 {
        return None;
    }

    let year = format!("20{}", &date_teams[0..2]);
    let month_str = &date_teams[2..5].to_uppercase();
    let day = &date_teams[5..7];

    let month = match month_str.as_str() {
        "JAN" => "01",
        "FEB" => "02",
        "MAR" => "03",
        "APR" => "04",
        "MAY" => "05",
        "JUN" => "06",
        "JUL" => "07",
        "AUG" => "08",
        "SEP" => "09",
        "OCT" => "10",
        "NOV" => "11",
        "DEC" => "12",
        _ => return None,
    };

    Some(format!("{}-{}-{}", year, month, day))
}

/// Extract concatenated team codes from event ticker
fn extract_teams_concat(event_ticker: &str) -> Option<String> {
    let parts: Vec<&str> = event_ticker.split('-').collect();
    if parts.len() < 2 {
        return None;
    }

    let date_teams = parts[1];
    if date_teams.len() <= 7 {
        return None;
    }

    Some(date_teams[7..].to_string())
}

/// Try multiple split points to find valid team codes
fn try_splits(teams_concat: &str) -> Vec<(String, String)> {
    let len = teams_concat.len();
    let mut results = Vec::new();

    // Try splits from 2 to len-2
    for split in 2..=std::cmp::min(5, len.saturating_sub(2)) {
        if split < len {
            let t1 = teams_concat[..split].to_lowercase();
            let t2 = teams_concat[split..].to_lowercase();
            results.push((t1, t2));
        }
    }

    results
}

/// Generate slug variations to try for a team name
fn generate_slug_variations(team_name: &str) -> Vec<String> {
    let mut variations = Vec::new();
    let lower = team_name.to_lowercase();

    // Common patterns for college teams
    let cleaned = lower
        .replace("st.", "")
        .replace("state", "")
        .replace(" ", "")
        .replace("-", "")
        .replace("'", "");

    // Full lowercase name without spaces
    variations.push(lower.replace(" ", "").replace("-", "").replace("'", ""));

    // First word only
    if let Some(first_word) = lower.split_whitespace().next() {
        variations.push(first_word.to_string());
    }

    // Common abbreviation patterns
    if lower.contains("state") {
        let abbrev = lower.replace(" state", "st").replace(" ", "");
        variations.push(abbrev);
    }

    // Remove common suffixes
    for suffix in &["university", "college", "state", "tech"] {
        if cleaned.ends_with(suffix) {
            variations.push(cleaned.trim_end_matches(suffix).to_string());
        }
    }

    variations.into_iter().filter(|s| !s.is_empty()).collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Load existing cache
    let cache_path = std::path::Path::new("controller/kalshi_team_cache.json");
    let mut cache: TeamCacheFile = if cache_path.exists() {
        let contents = std::fs::read_to_string(cache_path)?;
        serde_json::from_str(&contents).unwrap_or_default()
    } else {
        TeamCacheFile::default()
    };

    println!("Loaded {} existing mappings", cache.mappings.len());

    // Fetch Kalshi NCAAMB events
    println!("\n📡 Fetching Kalshi NCAAMB events...");

    let url = format!(
        "{}/markets?series_ticker=KXNCAAMBGAME&limit=500",
        KALSHI_API_BASE
    );

    let resp: KalshiMarketsResponse = client.get(&url).send().await?.json().await?;
    println!("  Found {} markets", resp.markets.len());

    // Group by event to get team info
    let mut events: HashMap<String, Vec<&KalshiMarket>> = HashMap::new();
    for market in &resp.markets {
        events.entry(market.event_ticker.clone())
            .or_default()
            .push(market);
    }

    println!("  {} unique events", events.len());

    // Extract team mappings
    let mut kalshi_teams: HashMap<String, String> = HashMap::new(); // code -> name
    let mut discovered_mappings: Vec<(String, String, String)> = Vec::new(); // (kalshi, poly, name)
    let mut unmatched_teams: Vec<UnmatchedTeam> = Vec::new();
    let mut matched_events = 0;
    let mut unmatched_events = 0;

    println!("\n🔍 Probing Polymarket for matches...\n");

    for (event_ticker, markets) in &events {
        // Get date
        let date = match parse_date(event_ticker) {
            Some(d) => d,
            None => continue,
        };

        // Get team codes from market suffixes
        let mut team_codes: HashSet<String> = HashSet::new();
        let mut team_names: HashMap<String, String> = HashMap::new();

        for market in markets {
            if let Some(suffix) = extract_suffix(&market.ticker) {
                team_codes.insert(suffix.clone());
                if let Some(name) = &market.yes_sub_title {
                    kalshi_teams.insert(suffix.to_lowercase(), name.clone());
                    team_names.insert(suffix, name.clone());
                }
            }
        }

        let codes: Vec<String> = team_codes.into_iter().collect();
        if codes.len() != 2 {
            continue;
        }

        // Try identity mapping first (Kalshi codes lowercased)
        let slug1 = format!("cbb-{}-{}-{}", codes[0].to_lowercase(), codes[1].to_lowercase(), date);
        let slug2 = format!("cbb-{}-{}-{}", codes[1].to_lowercase(), codes[0].to_lowercase(), date);

        let mut found = false;

        for slug in &[slug1, slug2] {
            let url = format!("{}/events?slug={}", GAMMA_API_BASE, slug);
            if let Ok(resp) = client.get(&url).send().await {
                if let Ok(events) = resp.json::<Vec<PolyEvent>>().await {
                    if let Some(event) = events.first() {
                        if event.slug.is_some() {
                            matched_events += 1;
                            found = true;
                            // Identity mapping - codes match, no explicit mapping needed
                            break;
                        }
                    }
                }
            }
        }

        if found {
            continue;
        }

        // Try alternative splits from concatenated teams
        if let Some(teams_concat) = extract_teams_concat(event_ticker) {
            for (t1, t2) in try_splits(&teams_concat) {
                let slug = format!("cbb-{}-{}-{}", t1, t2, date);
                let url = format!("{}/events?slug={}", GAMMA_API_BASE, slug);

                if let Ok(resp) = client.get(&url).send().await {
                    if let Ok(events) = resp.json::<Vec<PolyEvent>>().await {
                        if let Some(event) = events.first() {
                            if let Some(poly_slug) = &event.slug {
                                // Extract poly codes from slug
                                let parts: Vec<&str> = poly_slug.split('-').collect();
                                if parts.len() >= 4 {
                                    let poly_t1 = parts[1].to_string();
                                    let poly_t2 = parts[2].to_string();

                                    // Check if these differ from Kalshi codes
                                    for kalshi_code in &codes {
                                        let kalshi_lower = kalshi_code.to_lowercase();
                                        if kalshi_lower != poly_t1 && kalshi_lower != poly_t2 {
                                            // Found a non-identity mapping
                                            // Try to match by position
                                            if let Some(name) = team_names.get(kalshi_code) {
                                                // Check which poly code matches this team
                                                if let Some(title) = &event.title {
                                                    let title_lower = title.to_lowercase();
                                                    let name_lower = name.to_lowercase();

                                                    if title_lower.contains(&name_lower) ||
                                                       name_lower.contains(&poly_t1) ||
                                                       name_lower.contains(&poly_t2) {
                                                        // Determine mapping
                                                        let poly_code = if title_lower.starts_with(&name_lower.split_whitespace().next().unwrap_or("")) {
                                                            &poly_t1
                                                        } else {
                                                            &poly_t2
                                                        };

                                                        discovered_mappings.push((
                                                            kalshi_lower.clone(),
                                                            poly_code.clone(),
                                                            name.clone()
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                matched_events += 1;
                                found = true;
                                break;
                            }
                        }
                    }
                }
            }
        }

        if !found {
            unmatched_events += 1;
            if unmatched_events <= 10 {
                println!("  ❌ No match: {} ({:?})", event_ticker, codes);
            }

            // Store unmatched teams for later review
            for code in &codes {
                if let Some(name) = team_names.get(code) {
                    unmatched_teams.push(UnmatchedTeam {
                        kalshi_code: code.clone(),
                        team_name: name.clone(),
                        event_ticker: event_ticker.clone(),
                        date: date.clone(),
                    });
                }
            }
        }

        // Rate limit
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    println!("\n📊 Results:");
    println!("  Matched: {}", matched_events);
    println!("  Unmatched: {}", unmatched_events);
    println!("  Non-identity mappings found: {}", discovered_mappings.len());

    // Add discovered mappings to cache
    let mut added = 0;
    for (kalshi, poly, name) in &discovered_mappings {
        if kalshi != poly {
            let key = format!("cbb:{}", poly);
            if !cache.mappings.contains_key(&key) {
                println!("  + {} -> {} ({})", poly, kalshi, name);
                cache.mappings.insert(key, kalshi.clone());
                added += 1;
            }
        }
    }

    // Add some known mappings that we discovered earlier
    let known_mappings = [
        ("hawaii", "haw", "Hawaii"),
        ("csu", "csb", "CSU Bakersfield"),
        ("lamon", "ulm", "Louisiana-Monroe"),
        ("applst", "app", "Appalachian State"),
    ];

    for (poly, kalshi, name) in known_mappings {
        let key = format!("cbb:{}", poly);
        if !cache.mappings.contains_key(&key) {
            println!("  + {} -> {} ({}) [known]", poly, kalshi, name);
            cache.mappings.insert(key, kalshi.to_string());
            added += 1;
        }
    }

    println!("\n  Added {} new mappings", added);
    println!("  Total mappings: {}", cache.mappings.len());

    // Save cache
    let json = serde_json::to_string_pretty(&cache)?;
    std::fs::write(cache_path, json)?;
    println!("\n✅ Saved to {}", cache_path.display());

    // Save unmatched teams for manual review
    if !unmatched_teams.is_empty() {
        // Deduplicate by kalshi_code
        let mut unique_teams: HashMap<String, UnmatchedTeam> = HashMap::new();
        for team in unmatched_teams {
            unique_teams.entry(team.kalshi_code.clone()).or_insert(team);
        }

        let mut sorted_teams: Vec<_> = unique_teams.into_values().collect();
        sorted_teams.sort_by(|a, b| a.kalshi_code.cmp(&b.kalshi_code));

        let unmatched_path = std::path::Path::new("controller/unmatched_ncaamb_teams.json");
        let unmatched_json = serde_json::to_string_pretty(&sorted_teams)?;
        std::fs::write(unmatched_path, &unmatched_json)?;
        println!("📝 Saved {} unmatched teams to {}", sorted_teams.len(), unmatched_path.display());
    }

    Ok(())
}
