//! Runtime path helpers for the remote trader.
//!
//! Similar to the controller, configuration/secrets often live in the workspace root,
//! even when the crate is executed from within `trader/`.

use std::path::{Path, PathBuf};

pub fn trader_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn workspace_root() -> PathBuf {
    trader_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(trader_dir)
}

/// Load `.env` once, searching common locations:
/// - current working directory
/// - workspace root (one folder up from `trader/`)
/// - trader crate dir
pub fn load_dotenv() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let mut candidates: Vec<PathBuf> = Vec::new();

        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(".env"));
            if let Some(parent) = cwd.parent() {
                candidates.push(parent.join(".env"));
            }
        }

        candidates.push(workspace_root().join(".env"));
        candidates.push(trader_dir().join(".env"));

        for p in candidates {
            if p.exists() && dotenvy::from_path(&p).is_ok() {
                tracing::debug!("Loaded .env from {}", p.display());
                return;
            }
        }

        let _ = dotenvy::dotenv();
    });
}

