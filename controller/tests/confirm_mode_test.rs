//! Integration tests for confirm mode

use arb_bot::config::{requires_confirmation, confirm_mode_skip_leagues};
use std::sync::Mutex;

// Mutex to serialize tests that modify CONFIRM_MODE_SKIP env var
static ENV_MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn test_requires_confirmation_default() {
    let _lock = ENV_MUTEX.lock().unwrap();

    // Clear any existing env var
    std::env::remove_var("CONFIRM_MODE_SKIP");

    // All leagues should require confirmation by default
    assert!(requires_confirmation("nba"));
    assert!(requires_confirmation("epl"));
    assert!(requires_confirmation("nfl"));
    assert!(requires_confirmation("unknown_league")); // Unknown defaults to true
}

#[test]
fn test_confirm_mode_skip() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "nba,nfl");

    // Skipped leagues should not require confirmation
    assert!(!requires_confirmation("nba"));
    assert!(!requires_confirmation("nfl"));

    // Other leagues still require confirmation
    assert!(requires_confirmation("epl"));
    assert!(requires_confirmation("lol"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_skip_case_insensitive() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "NBA,NFL");

    // Should work case-insensitively
    assert!(!requires_confirmation("nba"));
    assert!(!requires_confirmation("NBA"));
    assert!(!requires_confirmation("nfl"));
    assert!(!requires_confirmation("NFL"));

    // Other leagues still require confirmation
    assert!(requires_confirmation("epl"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_skip_with_whitespace() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "nba , nfl , epl");

    // Should handle whitespace correctly
    assert!(!requires_confirmation("nba"));
    assert!(!requires_confirmation("nfl"));
    assert!(!requires_confirmation("epl"));

    // Other leagues still require confirmation
    assert!(requires_confirmation("lol"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_skip_leagues_empty() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::remove_var("CONFIRM_MODE_SKIP");

    // Should return empty set when not configured
    let skip = confirm_mode_skip_leagues();
    assert!(skip.is_empty());
}

#[test]
fn test_confirm_mode_skip_leagues_parsing() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "cs2,lol,cod");

    let skip = confirm_mode_skip_leagues();
    assert_eq!(skip.len(), 3);
    assert!(skip.contains("cs2"));
    assert!(skip.contains("lol"));
    assert!(skip.contains("cod"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_skip_single_league() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "nba");

    // Only one league skipped
    assert!(!requires_confirmation("nba"));
    assert!(requires_confirmation("nfl"));
    assert!(requires_confirmation("epl"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_skip_all_soccer() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "epl,bundesliga,laliga,seriea,ligue1,ucl,uel,eflc,mls");

    // All soccer leagues skipped
    assert!(!requires_confirmation("epl"));
    assert!(!requires_confirmation("bundesliga"));
    assert!(!requires_confirmation("laliga"));
    assert!(!requires_confirmation("seriea"));
    assert!(!requires_confirmation("ligue1"));
    assert!(!requires_confirmation("ucl"));
    assert!(!requires_confirmation("uel"));
    assert!(!requires_confirmation("eflc"));
    assert!(!requires_confirmation("mls"));

    // US sports still require confirmation
    assert!(requires_confirmation("nba"));
    assert!(requires_confirmation("nfl"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_skip_all_us_sports() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "nba,nfl,nhl,mlb,ncaaf");

    // All US sports skipped
    assert!(!requires_confirmation("nba"));
    assert!(!requires_confirmation("nfl"));
    assert!(!requires_confirmation("nhl"));
    assert!(!requires_confirmation("mlb"));
    assert!(!requires_confirmation("ncaaf"));

    // Soccer still requires confirmation
    assert!(requires_confirmation("epl"));
    assert!(requires_confirmation("ligue1"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_skip_all_esports() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "cs2,lol,cod");

    // All esports skipped
    assert!(!requires_confirmation("cs2"));
    assert!(!requires_confirmation("lol"));
    assert!(!requires_confirmation("cod"));

    // Sports still require confirmation
    assert!(requires_confirmation("nba"));
    assert!(requires_confirmation("epl"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}

#[test]
fn test_confirm_mode_unknown_league_default() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::remove_var("CONFIRM_MODE_SKIP");

    // Unknown leagues should default to requiring confirmation
    assert!(requires_confirmation("unknown_league"));
    assert!(requires_confirmation("xyz"));
    assert!(requires_confirmation(""));
}

#[test]
fn test_confirm_mode_mixed_known_unknown() {
    let _lock = ENV_MUTEX.lock().unwrap();

    std::env::set_var("CONFIRM_MODE_SKIP", "nba,unknown_league");

    // Known league is skipped
    assert!(!requires_confirmation("nba"));

    // Unknown league in skip list is also skipped
    assert!(!requires_confirmation("unknown_league"));

    // Other known leagues still require confirmation
    assert!(requires_confirmation("nfl"));

    std::env::remove_var("CONFIRM_MODE_SKIP");
}
