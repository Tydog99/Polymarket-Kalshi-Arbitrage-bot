//! CLI tool to generate team code mappings from live Polymarket API data.
//!
//! Usage: cargo run --bin generate-team-cache
//!
//! This tool fetches active sports events from Polymarket and extracts
//! team code mappings by parsing event slugs.
//!
//! The cache format is: poly_code -> kalshi_code
//! For example: "epl:liv" -> "lfc" (Polymarket uses "liv", Kalshi uses "LFC")
//!
//! IMPORTANT: This tool discovers Polymarket codes. You must manually verify
//! the corresponding Kalshi codes match. Most are identity mappings, but some
//! differ (e.g., liv/lfc, mac/mci, ast/avl).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

// Sports series on Polymarket
// Format: (league_code, poly_prefix, series_id)
const SPORTS_SERIES: &[(&str, &str, &str)] = &[
    // Soccer
    ("epl", "epl", "10188"),           // Premier League
    ("bundesliga", "bun", "10189"),    // Bundesliga
    ("laliga", "lal", "10190"),        // La Liga
    ("seriea", "sea", "10191"),        // Serie A
    ("ligue1", "fl1", "10192"),        // Ligue 1
    ("ucl", "ucl", "10193"),           // Champions League
    ("uel", "uel", "10194"),           // Europa League
    ("eflc", "elc", "10195"),          // EFL Championship
    // US Sports
    ("nba", "nba", "10345"),           // NBA
    ("nfl", "nfl", "10187"),           // NFL
    ("nhl", "nhl", "10346"),           // NHL
    ("mlb", "mlb", "10347"),           // MLB
    ("mls", "mls", "10348"),           // MLS
    ("ncaaf", "cfb", "10349"),         // College Football
];

// Known Polymarket -> Kalshi code mappings that differ
// Format: (league_prefix, poly_code, kalshi_code)
const KNOWN_MAPPINGS: &[(&str, &str, &str)] = &[
    // EPL
    ("epl", "liv", "lfc"),      // Liverpool
    ("epl", "mac", "mci"),      // Manchester City
    ("epl", "ast", "avl"),      // Aston Villa
    ("epl", "not", "nfo"),      // Nottingham Forest
    ("epl", "che", "cfc"),      // Chelsea
    ("epl", "wes", "whu"),      // West Ham
    // NHL
    ("nhl", "lak", "la"),       // LA Kings
    ("nhl", "las", "vgk"),      // Vegas Golden Knights
    ("nhl", "mon", "mtl"),      // Montreal Canadiens
    ("nhl", "cal", "cgy"),      // Calgary Flames
    ("nhl", "utah", "uta"),     // Utah Hockey Club
];

#[derive(Debug, Deserialize)]
struct PolyEvent {
    slug: Option<String>,
    markets: Option<Vec<PolyMarket>>,
}

#[derive(Debug, Deserialize)]
struct PolyMarket {
    outcomes: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct TeamCacheFile {
    mappings: HashMap<String, String>,
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

    // Track new discoveries
    let mut new_mappings: HashMap<String, HashSet<String>> = HashMap::new();
    let mut discoveries: Vec<(String, String, String)> = Vec::new();

    for (league_code, poly_prefix, series_id) in SPORTS_SERIES {
        println!("\n📡 Fetching {} events (series {})...", league_code, series_id);

        let url = format!(
            "{}/events?series_id={}&closed=false&limit=100",
            GAMMA_API_BASE, series_id
        );

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                println!("  ⚠️ Failed to fetch: {}", e);
                continue;
            }
        };

        if !resp.status().is_success() {
            println!("  ⚠️ HTTP {}", resp.status());
            continue;
        }

        let events: Vec<PolyEvent> = match resp.json().await {
            Ok(e) => e,
            Err(e) => {
                println!("  ⚠️ Parse error: {}", e);
                continue;
            }
        };

        println!("  Found {} events", events.len());

        for event in events {
            let slug = match &event.slug {
                Some(s) => s,
                None => continue,
            };

            // Parse team codes from event slug
            // Format: {prefix}-{team1}-{team2}-{date} or {prefix}-{team1}-{team2}-{date}-{suffix}
            let parts: Vec<&str> = slug.split('-').collect();
            if parts.len() < 4 {
                continue;
            }

            // Verify prefix matches
            if parts[0] != *poly_prefix {
                continue;
            }

            let team1_code = parts[1].to_lowercase();
            let team2_code = parts[2].to_lowercase();

            // Skip if codes look like dates or numbers
            if team1_code.chars().all(|c| c.is_numeric())
                || team2_code.chars().all(|c| c.is_numeric()) {
                continue;
            }

            // Extract team names from market outcomes if available
            let mut team1_name: Option<String> = None;
            let mut team2_name: Option<String> = None;

            if let Some(markets) = &event.markets {
                for market in markets {
                    if let Some(outcomes_str) = &market.outcomes {
                        if let Ok(outcomes) = serde_json::from_str::<Vec<String>>(outcomes_str) {
                            // Skip non-team outcomes like "Yes"/"No", "Over"/"Under"
                            if outcomes.len() >= 2
                                && !outcomes[0].eq_ignore_ascii_case("yes")
                                && !outcomes[0].eq_ignore_ascii_case("over")
                                && !outcomes[0].eq_ignore_ascii_case("draw") {
                                if team1_name.is_none() {
                                    team1_name = Some(outcomes[0].clone());
                                }
                                // Handle draw case - team2 might be at index 2
                                if team2_name.is_none() {
                                    if outcomes.len() > 2 && outcomes.iter().any(|o| o.eq_ignore_ascii_case("draw")) {
                                        // Find non-draw, non-team1 outcome
                                        for o in &outcomes {
                                            if !o.eq_ignore_ascii_case("draw") && Some(o.clone()) != team1_name {
                                                team2_name = Some(o.clone());
                                                break;
                                            }
                                        }
                                    } else if outcomes.len() >= 2 {
                                        team2_name = Some(outcomes[1].clone());
                                    }
                                }
                            }
                        }
                    }
                    if team1_name.is_some() && team2_name.is_some() {
                        break;
                    }
                }
            }

            // Record discovered Polymarket codes
            // Look up known mapping or use identity
            let kalshi1 = KNOWN_MAPPINGS.iter()
                .find(|(p, poly, _)| *p == *poly_prefix && *poly == team1_code)
                .map(|(_, _, k)| k.to_string())
                .unwrap_or_else(|| team1_code.clone());

            let kalshi2 = KNOWN_MAPPINGS.iter()
                .find(|(p, poly, _)| *p == *poly_prefix && *poly == team2_code)
                .map(|(_, _, k)| k.to_string())
                .unwrap_or_else(|| team2_code.clone());

            let key1 = format!("{}:{}", poly_prefix, team1_code);
            let key2 = format!("{}:{}", poly_prefix, team2_code);

            new_mappings.entry(key1.clone()).or_default().insert(kalshi1.clone());
            new_mappings.entry(key2.clone()).or_default().insert(kalshi2.clone());

            if let Some(name) = &team1_name {
                discoveries.push((league_code.to_string(), team1_code.clone(), name.clone()));
            }
            if let Some(name) = &team2_name {
                discoveries.push((league_code.to_string(), team2_code.clone(), name.clone()));
            }
        }

        // Small delay between leagues
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Print discoveries
    println!("\n\n📋 Discovered Team Codes:\n");
    println!("{:<12} {:<8} {}", "League", "Code", "Name");
    println!("{}", "-".repeat(50));

    let mut sorted_discoveries = discoveries.clone();
    sorted_discoveries.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    sorted_discoveries.dedup();

    for (league, code, name) in &sorted_discoveries {
        println!("{:<12} {:<8} {}", league, code, name);
    }

    // Update cache with new mappings
    // The key is poly_code, value is kalshi_code
    let mut added = 0;
    let mut fixed = 0;

    for (key, kalshi_codes) in &new_mappings {
        // key is like "epl:liv", kalshi_codes contains the kalshi code(s)
        if let Some(kalshi_code) = kalshi_codes.iter().next() {
            let existing = cache.mappings.get(key);

            if existing.is_none() {
                cache.mappings.insert(key.clone(), kalshi_code.to_string());
                added += 1;
            } else if existing != Some(kalshi_code) {
                let old = existing.unwrap();
                println!("⚠️  Fixing {}: {} (was {})", key, kalshi_code, old);
                cache.mappings.insert(key.clone(), kalshi_code.to_string());
                fixed += 1;
            }
        }
    }

    // Also remove incorrect mappings that conflict:
    // 1. Long-form mappings (e.g., "epl:manchester_united" when we have "epl:mun")
    // 2. Identity mappings where a different poly_code maps to the same kalshi (e.g., remove
    //    "epl:lfc -> lfc" when we have "epl:liv -> lfc")
    let keys_to_remove: Vec<String> = cache.mappings.keys()
        .filter(|k| {
            if let Some((prefix, poly_code)) = k.split_once(':') {
                let kalshi = cache.mappings.get(*k);
                if let Some(kalshi_code) = kalshi {
                    // Check if this is an identity mapping (poly_code == kalshi_code)
                    let is_identity = poly_code == kalshi_code.as_str();

                    // Look for another code with the same kalshi mapping
                    for (other_key, other_kalshi) in &cache.mappings {
                        if other_key != *k
                            && other_key.starts_with(&format!("{}:", prefix))
                            && other_kalshi == kalshi_code
                        {
                            if let Some((_, other_poly)) = other_key.split_once(':') {
                                // Remove if: identity mapping with a different poly_code for same kalshi
                                if is_identity && other_poly != kalshi_code.as_str() {
                                    return true;
                                }
                                // Remove if: long-form code (has underscore) with shorter alternative
                                if poly_code.len() > other_poly.len() && poly_code.contains('_') {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
            false
        })
        .cloned()
        .collect();

    for key in &keys_to_remove {
        println!("🗑️  Removing conflicting entry: {}", key);
        cache.mappings.remove(key);
    }

    println!("\n\n📊 Summary:");
    println!("  New mappings added: {}", added);
    println!("  Mappings fixed: {}", fixed);
    println!("  Total mappings: {}", cache.mappings.len());

    // Also add normalized name variants
    // e.g., for "Arsenal" with code "ars", add "epl:arsenal" -> "ars"
    for (league, code, name) in &sorted_discoveries {
        let prefix = SPORTS_SERIES.iter()
            .find(|(l, _, _)| l == league)
            .map(|(_, p, _)| *p)
            .unwrap_or(league.as_str());

        let normalized = name.to_lowercase()
            .replace(' ', "_")
            .replace(".", "")
            .replace("'", "");

        let name_key = format!("{}:{}", prefix, normalized);
        if !cache.mappings.contains_key(&name_key) && normalized != *code {
            cache.mappings.insert(name_key, code.clone());
        }
    }

    // Save updated cache
    let json = serde_json::to_string_pretty(&cache)?;
    std::fs::write(cache_path, json)?;
    println!("\n✅ Saved to {}", cache_path.display());

    Ok(())
}
