//! JSONL-based tracker for A/B test strategy results.
//!
//! Appends one JSON object per line to `strategy_results.jsonl`, colocated
//! with `positions.json` in the workspace root.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use tracing::warn;

use crate::strategy::StrategyResult;

/// Append-only JSONL writer for strategy execution results.
pub struct StrategyTracker {
    path: PathBuf,
}

impl StrategyTracker {
    /// Create a new tracker writing to `strategy_results.jsonl` next to `positions.json`.
    pub fn new() -> Self {
        let path = crate::paths::resolve_workspace_file("strategy_results.jsonl");
        Self { path }
    }

    /// Append a single result as one JSON line.
    pub fn record(&self, result: &StrategyResult) {
        let line = match serde_json::to_string(result) {
            Ok(json) => json,
            Err(e) => {
                warn!("[STRATEGY] Failed to serialize result: {}", e);
                return;
            }
        };
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{}", line) {
                    warn!("[STRATEGY] Failed to write result: {}", e);
                }
            }
            Err(e) => {
                warn!("[STRATEGY] Failed to open {}: {}", self.path.display(), e);
            }
        }
    }
}
