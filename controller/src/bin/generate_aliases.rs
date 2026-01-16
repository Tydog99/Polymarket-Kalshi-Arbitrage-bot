//! CLI tool to generate esports team aliases from live Polymarket API data.
//!
//! Usage: cargo run --bin generate-aliases
//!
//! This tool fetches active esports events from Polymarket and extracts
//! team name -> code mappings that can be added to ESPORTS_TEAM_ALIASES.
//!
//! It extracts codes from TWO sources:
//! 1. The event slug (e.g., "lol-fur-lll-2026-01-17" -> codes "fur", "lll")
//! 2. The market outcomes array (e.g., ["FURIA Esports", "LOUD"])

use anyhow::Result;
use std::collections::{HashMap, HashSet};

// Polymarket Gamma API base URL
const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

// Esports series IDs on Polymarket
const ESPORTS_SERIES: &[(&str, &str)] = &[
    ("CS2", "10310"),
    ("LoL", "10311"),
    ("CoD", "10427"),
];

#[derive(Debug, serde::Deserialize)]
struct PolyEvent {
    slug: Option<String>,
    title: Option<String>,
    markets: Option<Vec<PolyMarket>>,
}

#[derive(Debug, serde::Deserialize)]
struct PolyMarket {
    slug: Option<String>,
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<String>,
    outcomes: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Collect all discovered aliases: normalized_name -> set of codes
    let mut all_aliases: HashMap<String, HashSet<String>> = HashMap::new();

    // Track what we found for display
    let mut discoveries: Vec<(String, String, String, String, String)> = Vec::new();

    for (game, series_id) in ESPORTS_SERIES {
        println!("\n{} Fetching {} markets (series {})...", "📡", game, series_id);

        let url = format!(
            "{}/events?series_id={}&closed=false&limit=100",
            GAMMA_API_BASE, series_id
        );

        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            println!("  {} Failed to fetch: {}", "⚠️", resp.status());
            continue;
        }

        let events: Vec<PolyEvent> = resp.json().await?;
        println!("  Found {} events", events.len());

        for event in events {
            let slug = match &event.slug {
                Some(s) => s,
                None => continue,
            };

            let title = match &event.title {
                Some(t) => t,
                None => continue,
            };

            // Parse team names from title (e.g., "CS2: Team A vs Team B (BO3)")
            let teams = match parse_teams_from_title(title) {
                Some(t) => t,
                None => continue,
            };

            // Extract codes from slug (e.g., "lol-fur-lll-2026-01-17" -> ["fur", "lll"])
            let slug_codes = extract_codes_from_slug(slug);

            if slug_codes.len() >= 2 {
                // We have codes from slug - match them to teams by position
                // Slug format: game-code1-code2-date
                let (code1, code2) = (&slug_codes[0], &slug_codes[1]);
                let (team1, team2) = &teams;

                discoveries.push((
                    game.to_string(),
                    title.clone(),
                    slug.clone(),
                    format!("{} -> {}", code1, team1),
                    format!("{} -> {}", code2, team2),
                ));

                // Add mappings
                let norm1 = normalize_team(team1);
                let norm2 = normalize_team(team2);

                all_aliases.entry(norm1).or_default().insert(code1.to_lowercase());
                all_aliases.entry(norm2).or_default().insert(code2.to_lowercase());
            }

            // Also check market outcomes for additional codes
            if let Some(markets) = &event.markets {
                for market in markets {
                    let market_slug = market.slug.as_deref().unwrap_or("");

                    // Skip non-moneyline markets
                    if market_slug.contains("-game")
                        || market_slug.contains("-total")
                        || market_slug.contains("-map-")
                        || market_slug.contains("-handicap")
                    {
                        continue;
                    }

                    // Parse outcomes
                    let outcomes: Vec<String> = market.outcomes
                        .as_ref()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_default();

                    if outcomes.len() >= 2 {
                        // Check if outcomes are short codes (different from full names)
                        let (team1, team2) = &teams;
                        let outcome1_is_code = is_likely_code(&outcomes[0], team1);
                        let outcome2_is_code = is_likely_code(&outcomes[1], team2);

                        if outcome1_is_code {
                            let norm = normalize_team(team1);
                            all_aliases.entry(norm).or_default().insert(outcomes[0].to_lowercase());
                        }
                        if outcome2_is_code {
                            let norm = normalize_team(team2);
                            all_aliases.entry(norm).or_default().insert(outcomes[1].to_lowercase());
                        }
                    }

                    break; // Only process first moneyline market per event
                }
            }
        }
    }

    // Display discoveries
    println!("\n{} Discovered mappings from slugs:", "🔍");
    println!("{}", "=".repeat(70));

    for (game, title, slug, mapping1, mapping2) in &discoveries {
        println!("  [{}] {}", game, title);
        println!("       Slug: {}", slug);
        println!("       {} | {}", mapping1, mapping2);
    }

    // Output as Rust code
    println!("\n{} Suggested additions to ESPORTS_TEAM_ALIASES:", "📝");
    println!("{}", "=".repeat(70));

    let mut sorted_aliases: Vec<_> = all_aliases.into_iter().collect();
    sorted_aliases.sort_by(|a, b| a.0.cmp(&b.0));

    for (team, codes) in sorted_aliases {
        // Filter out codes that are same as normalized name or too long
        let useful_codes: Vec<_> = codes.into_iter()
            .filter(|c| c != &team && c.len() <= 6)
            .collect();

        if useful_codes.is_empty() {
            continue;
        }

        let mut all_codes: Vec<String> = useful_codes;
        all_codes.sort();

        let codes_str = all_codes.iter()
            .map(|c| format!("\"{}\"", c))
            .collect::<Vec<_>>()
            .join(", ");

        println!("    (\"{}\", &[{}]),", team, codes_str);
    }

    println!("\n{} Done! Review and add relevant entries to discovery.rs", "✅");

    Ok(())
}

/// Extract team codes from Polymarket slug
/// e.g., "lol-fur-lll-2026-01-17" -> ["fur", "lll"]
/// e.g., "cs2-m8-tl1-2026-01-17" -> ["m8", "tl1"]
fn extract_codes_from_slug(slug: &str) -> Vec<String> {
    let parts: Vec<&str> = slug.split('-').collect();

    // Expected format: game-code1-code2-YYYY-MM-DD[-suffix]
    // Need at least: game + 2 codes + 3 date parts = 6 parts
    if parts.len() < 6 {
        return vec![];
    }

    // Find the date part (YYYY) - it's 4 digits
    let date_idx = parts.iter().position(|p| p.len() == 4 && p.chars().all(|c| c.is_ascii_digit()));

    match date_idx {
        Some(idx) if idx >= 3 => {
            // Codes are between game prefix (index 0) and date
            // e.g., ["lol", "fur", "lll", "2026", "01", "17"]
            //         0      1      2      3       4     5
            let codes: Vec<String> = parts[1..idx].iter().map(|s| s.to_string()).collect();
            if codes.len() >= 2 {
                codes
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Check if an outcome string is likely a short code rather than full team name
fn is_likely_code(outcome: &str, full_name: &str) -> bool {
    let outcome_clean = outcome.trim();
    let name_clean = full_name.trim();

    // If outcome is much shorter than full name and doesn't match, it's probably a code
    outcome_clean.len() <= 5
        && outcome_clean.to_lowercase() != name_clean.to_lowercase()
        && !name_clean.to_lowercase().starts_with(&outcome_clean.to_lowercase())
}

/// Parse team names from event title
/// e.g., "Counter-Strike: FURIA vs 9INE (BO3)" -> ["FURIA", "9INE"]
fn parse_teams_from_title(title: &str) -> Option<(String, String)> {
    // Try pattern: "Game: Team1 vs Team2 (format)"
    let vs_patterns = [" vs. ", " vs "];

    for vs in vs_patterns {
        if let Some(vs_pos) = title.to_lowercase().find(&vs.to_lowercase()) {
            // Find start of team1 (after colon or start)
            let before_vs = &title[..vs_pos];
            let team1_start = before_vs.rfind(": ").map(|p| p + 2).unwrap_or(0);
            let team1 = before_vs[team1_start..].trim();

            // Find end of team2 (before parenthesis or end)
            let after_vs = &title[vs_pos + vs.len()..];
            let team2_end = after_vs.find(" (").unwrap_or(after_vs.len());
            let team2 = after_vs[..team2_end].trim();

            if !team1.is_empty() && !team2.is_empty() {
                return Some((team1.to_string(), team2.to_string()));
            }
        }
    }

    None
}

/// Normalize team name for use as canonical key
fn normalize_team(name: &str) -> String {
    let lower = name.to_lowercase();

    // Remove common suffixes
    let cleaned = lower
        .trim_end_matches(" esports")
        .trim_end_matches(" gaming")
        .trim_end_matches(" team")
        .trim_end_matches(" clan")
        .trim_start_matches("team ")
        .trim_start_matches("clan ");

    // Replace spaces with hyphens, remove special chars
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .replace(".", "")
        .replace("'", "")
}

/// Match outcome codes to team names
/// Returns vec of (code, team_name) pairs
fn match_outcomes_to_teams(outcomes: &[String], teams: &(String, String)) -> Vec<(String, String)> {
    let (team1, team2) = teams;
    let mut mappings = Vec::new();

    for outcome in outcomes {
        let outcome_lower = outcome.to_lowercase().trim().to_string();
        let outcome_clean = outcome_lower
            .trim_end_matches(" esports")
            .trim_end_matches(" gaming");

        // Try to match to team1
        let team1_lower = team1.to_lowercase();
        let team1_clean = team1_lower
            .trim_end_matches(" esports")
            .trim_end_matches(" gaming");

        // Try to match to team2
        let team2_lower = team2.to_lowercase();
        let team2_clean = team2_lower
            .trim_end_matches(" esports")
            .trim_end_matches(" gaming");

        // Strategy 1: Exact match (after cleaning)
        if outcome_clean == team1_clean {
            mappings.push((outcome.clone(), team1.clone()));
            continue;
        }
        if outcome_clean == team2_clean {
            mappings.push((outcome.clone(), team2.clone()));
            continue;
        }

        // Strategy 2: Prefix match (FUR -> FURIA)
        if team1_clean.starts_with(&outcome_clean) && !team2_clean.starts_with(&outcome_clean) {
            mappings.push((outcome.clone(), team1.clone()));
            continue;
        }
        if team2_clean.starts_with(&outcome_clean) && !team1_clean.starts_with(&outcome_clean) {
            mappings.push((outcome.clone(), team2.clone()));
            continue;
        }

        // Strategy 3: Team is prefix of outcome (rare but possible)
        if outcome_clean.starts_with(&team1_clean) && !outcome_clean.starts_with(&team2_clean) {
            mappings.push((outcome.clone(), team1.clone()));
            continue;
        }
        if outcome_clean.starts_with(&team2_clean) && !outcome_clean.starts_with(&team1_clean) {
            mappings.push((outcome.clone(), team2.clone()));
            continue;
        }

        // Strategy 4: Containment
        if team1_clean.contains(&outcome_clean) && !team2_clean.contains(&outcome_clean) {
            mappings.push((outcome.clone(), team1.clone()));
            continue;
        }
        if team2_clean.contains(&outcome_clean) && !team1_clean.contains(&outcome_clean) {
            mappings.push((outcome.clone(), team2.clone()));
            continue;
        }

        // If we can't match, still record it as unknown
        // (position-based: assume outcomes[0] = team1, outcomes[1] = team2)
        if outcomes.iter().position(|o| o == outcome) == Some(0) {
            mappings.push((outcome.clone(), team1.clone()));
        } else {
            mappings.push((outcome.clone(), team2.clone()));
        }
    }

    mappings
}
