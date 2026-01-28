//! Runtime path helpers.
//!
//! The repo is structured as a workspace with `controller/` as a crate.
//! Some resources (like `kalshi_team_cache.json`) live in the controller crate
//! directory, while user configuration/secrets (like `.env` and PEM keys) often
//! live in the workspace root (one level above the controller directory).

use std::path::{Path, PathBuf};

/// Absolute path to the `controller/` crate directory (compile-time).
pub fn controller_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Absolute path to the workspace root (best-effort).
///
/// In this repo layout, it's the parent directory of `controller/`.
pub fn workspace_root() -> PathBuf {
    controller_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(controller_dir)
}

/// Resolve a relative path, checking cwd first (for containers), then fallback.
///
/// - `must_exist`: if true, only return cwd path if file exists there
/// - `fallback`: compile-time fallback directory
fn resolve_with_cwd_priority<P: AsRef<Path>>(rel: P, must_exist: bool, fallback: PathBuf) -> PathBuf {
    let rel = rel.as_ref();

    // Check current working directory first (container deployment)
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_path = cwd.join(rel);
        if !must_exist || cwd_path.exists() {
            return cwd_path;
        }
    }

    // Fall back to compile-time directory (development)
    fallback.join(rel)
}

/// Resolve a path that should live in the controller crate directory.
///
/// Checks cwd first (if file exists), then controller crate directory.
pub fn resolve_controller_asset<P: AsRef<Path>>(rel: P) -> PathBuf {
    resolve_with_cwd_priority(rel, true, controller_dir())
}

/// Resolve a path that should live in the workspace root (secrets/config).
///
/// Checks cwd first (even if file doesn't exist - for writing), then workspace root.
pub fn resolve_workspace_file<P: AsRef<Path>>(rel: P) -> PathBuf {
    resolve_with_cwd_priority(rel, false, workspace_root())
}

/// Load `.env` once, searching common locations:
/// - current working directory
/// - workspace root (one folder up from `controller/`)
/// - controller crate dir
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

        candidates.push(resolve_workspace_file(".env"));
        candidates.push(resolve_controller_asset(".env"));

        for p in candidates {
            if p.exists() && dotenvy::from_path(&p).is_ok() {
                tracing::debug!("Loaded .env from {}", p.display());
                return;
            }
        }

        // Fallback: whatever dotenvy considers default.
        let _ = dotenvy::dotenv();
    });
}

/// Resolve a user-supplied path that may be relative to either:
/// - the process cwd
/// - the workspace root
/// - the controller dir
pub fn resolve_user_path<P: AsRef<Path>>(p: P) -> PathBuf {
    let p = p.as_ref();

    if p.is_absolute() && p.exists() {
        return p.to_path_buf();
    }

    // As-is (relative to cwd)
    if p.exists() {
        return p.to_path_buf();
    }

    // Relative to workspace root
    let ws = resolve_workspace_file(p);
    if ws.exists() {
        return ws;
    }

    // Relative to controller crate dir
    let ctrl = resolve_controller_asset(p);
    if ctrl.exists() {
        return ctrl;
    }

    // Best-effort fallback
    p.to_path_buf()
}

