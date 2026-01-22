//! Confirmation logging for audit trail of user decisions.
//!
//! Records all confirmation decisions (accept/reject/blacklist) to a timestamped
//! JSON file for later analysis.

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use crate::types::ArbType;

/// Status of a confirmation decision
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationStatus {
    /// User approved and arb was still valid
    Accepted,
    /// User approved but prices had moved (arb expired)
    AcceptedExpired,
    /// User rejected (can re-queue on next detection)
    Rejected,
    /// User rejected with 5-minute blacklist
    Blacklisted,
    /// Prices invalidated while pending (auto-removed)
    Expired,
}

/// A single confirmation record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationRecord {
    pub timestamp: DateTime<Utc>,
    pub status: ConfirmationStatus,
    pub market_id: u16,
    pub pair_id: String,
    pub description: String,
    pub league: String,
    pub arb_type: ArbType,
    pub yes_price_cents: u16,
    pub no_price_cents: u16,
    pub profit_cents: i16,
    pub max_contracts: u16,
    pub detection_count: u32,
    pub kalshi_url: String,
    pub poly_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Logger that appends confirmation records to a session file
pub struct ConfirmationLogger {
    writer: BufWriter<std::fs::File>,
    file_path: PathBuf,
    record_count: usize,
}

impl ConfirmationLogger {
    /// Create a new logger with a timestamped filename in .confirmations/
    pub fn new() -> Result<Self> {
        let dir = PathBuf::from(".confirmations");
        std::fs::create_dir_all(&dir)?;

        let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
        let filename = format!("confirmations_{}.json", timestamp);
        let file_path = dir.join(&filename);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;

        let writer = BufWriter::new(file);

        Ok(Self {
            writer,
            file_path,
            record_count: 0,
        })
    }

    /// Log a confirmation record
    pub fn log(&mut self, record: ConfirmationRecord) -> Result<()> {
        let json = serde_json::to_string(&record)?;
        writeln!(self.writer, "{}", json)?;
        self.writer.flush()?;
        self.record_count += 1;
        Ok(())
    }

    /// Get the path to the log file
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }

    /// Get the number of records logged.
    /// Useful for summary statistics when closing a confirmation session.
    #[allow(dead_code)]
    pub fn record_count(&self) -> usize {
        self.record_count
    }
}
