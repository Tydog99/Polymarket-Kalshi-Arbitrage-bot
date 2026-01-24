//! CLI tool to fetch all Polymarket CBB (NCAA Men's Basketball) events.
//!
//! Usage: cargo run --bin fetch-poly-cbb
//!
//! This tool probes Polymarket using known Kalshi team codes to discover
//! all available CBB events and outputs them in JSON format.

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
}

#[derive(Debug, Deserialize)]
struct PolyEvent {
    slug: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct PolyCbbTeam {
    poly_code: String,
    team_name: String,
    event_slug: String,
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

    for split in 2..=std::cmp::min(6, len.saturating_sub(2)) {
        if split < len {
            let t1 = teams_concat[..split].to_lowercase();
            let t2 = teams_concat[split..].to_lowercase();
            results.push((t1, t2));
        }
    }

    results
}

/// Extract team name from event title
/// "Louisiana-Monroe Warhawks vs. Appalachian State Mountaineers" -> ("Louisiana-Monroe Warhawks", "Appalachian State Mountaineers")
fn parse_title(title: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = title.split(" vs. ").collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    println!("📡 Fetching Kalshi NCAAMB events to use as probe list...");

    let url = format!(
        "{}/markets?series_ticker=KXNCAAMBGAME&limit=500",
        KALSHI_API_BASE
    );

    let resp: KalshiMarketsResponse = client.get(&url).send().await?.json().await?;
    println!("  Found {} Kalshi markets", resp.markets.len());

    // Group by event to get unique games
    let mut events: HashMap<String, Vec<&KalshiMarket>> = HashMap::new();
    for market in &resp.markets {
        events.entry(market.event_ticker.clone())
            .or_default()
            .push(market);
    }

    println!("  {} unique Kalshi events", events.len());
    println!("\n🔍 Probing Polymarket for CBB events...\n");

    let mut poly_teams: Vec<PolyCbbTeam> = Vec::new();
    let mut seen_slugs: HashSet<String> = HashSet::new();
    let mut matched = 0;
    let mut checked = 0;

    for (event_ticker, markets) in &events {
        checked += 1;

        // Get date
        let date = match parse_date(event_ticker) {
            Some(d) => d,
            None => continue,
        };

        // Get team codes from market suffixes
        let mut team_codes: HashSet<String> = HashSet::new();
        for market in markets {
            if let Some(suffix) = extract_suffix(&market.ticker) {
                team_codes.insert(suffix);
            }
        }

        let codes: Vec<String> = team_codes.into_iter().collect();
        if codes.len() != 2 {
            continue;
        }

        // Try identity mapping first (Kalshi codes lowercased)
        let slug1 = format!("cbb-{}-{}-{}", codes[0].to_lowercase(), codes[1].to_lowercase(), date);
        let slug2 = format!("cbb-{}-{}-{}", codes[1].to_lowercase(), codes[0].to_lowercase(), date);

        for slug in &[slug1, slug2] {
            if seen_slugs.contains(slug) {
                continue;
            }

            let url = format!("{}/events?slug={}", GAMMA_API_BASE, slug);
            if let Ok(resp) = client.get(&url).send().await {
                if let Ok(events) = resp.json::<Vec<PolyEvent>>().await {
                    if let Some(event) = events.first() {
                        if let (Some(event_slug), Some(title)) = (&event.slug, &event.title) {
                            seen_slugs.insert(event_slug.clone());
                            matched += 1;

                            // Extract team codes and names from slug and title
                            let slug_parts: Vec<&str> = event_slug.split('-').collect();
                            if slug_parts.len() >= 4 {
                                let poly_t1 = slug_parts[1].to_string();
                                let poly_t2 = slug_parts[2].to_string();

                                if let Some((name1, name2)) = parse_title(title) {
                                    poly_teams.push(PolyCbbTeam {
                                        poly_code: poly_t1.clone(),
                                        team_name: name1,
                                        event_slug: event_slug.clone(),
                                        date: date.clone(),
                                    });
                                    poly_teams.push(PolyCbbTeam {
                                        poly_code: poly_t2.clone(),
                                        team_name: name2,
                                        event_slug: event_slug.clone(),
                                        date: date.clone(),
                                    });

                                    println!("  ✅ {} - {}", event_slug, title);
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        // If identity didn't work, try alternative splits
        if !seen_slugs.iter().any(|s| s.contains(&date)) {
            if let Some(teams_concat) = extract_teams_concat(event_ticker) {
                for (t1, t2) in try_splits(&teams_concat) {
                    let slug = format!("cbb-{}-{}-{}", t1, t2, date);
                    if seen_slugs.contains(&slug) {
                        continue;
                    }

                    let url = format!("{}/events?slug={}", GAMMA_API_BASE, slug);
                    if let Ok(resp) = client.get(&url).send().await {
                        if let Ok(events) = resp.json::<Vec<PolyEvent>>().await {
                            if let Some(event) = events.first() {
                                if let (Some(event_slug), Some(title)) = (&event.slug, &event.title) {
                                    seen_slugs.insert(event_slug.clone());
                                    matched += 1;

                                    let slug_parts: Vec<&str> = event_slug.split('-').collect();
                                    if slug_parts.len() >= 4 {
                                        let poly_t1 = slug_parts[1].to_string();
                                        let poly_t2 = slug_parts[2].to_string();

                                        if let Some((name1, name2)) = parse_title(title) {
                                            poly_teams.push(PolyCbbTeam {
                                                poly_code: poly_t1.clone(),
                                                team_name: name1,
                                                event_slug: event_slug.clone(),
                                                date: date.clone(),
                                            });
                                            poly_teams.push(PolyCbbTeam {
                                                poly_code: poly_t2.clone(),
                                                team_name: name2,
                                                event_slug: event_slug.clone(),
                                                date: date.clone(),
                                            });

                                            println!("  ✅ {} - {}", event_slug, title);
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Rate limit
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Progress
        if checked % 50 == 0 {
            println!("  ... checked {}/{} events, found {} matches", checked, events.len(), matched);
        }
    }

    println!("\n📊 Results:");
    println!("  Kalshi events checked: {}", checked);
    println!("  Polymarket events found: {}", matched);
    println!("  Team entries: {}", poly_teams.len());

    // Deduplicate by poly_code, keeping first occurrence
    let mut unique_teams: HashMap<String, PolyCbbTeam> = HashMap::new();
    for team in poly_teams {
        unique_teams.entry(team.poly_code.clone()).or_insert(team);
    }

    let mut sorted_teams: Vec<_> = unique_teams.into_values().collect();
    sorted_teams.sort_by(|a, b| a.poly_code.cmp(&b.poly_code));

    println!("  Unique teams: {}", sorted_teams.len());

    // Save to file
    let output_path = std::path::Path::new("controller/poly_cbb_teams.json");
    let json = serde_json::to_string_pretty(&sorted_teams)?;
    std::fs::write(output_path, &json)?;
    println!("\n✅ Saved to {}", output_path.display());

    Ok(())
}
