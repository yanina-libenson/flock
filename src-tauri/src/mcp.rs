//! Self-contained install of Flock's own MCP server, so an orchestrator session
//! can be auto-wired with the `task_*` / `kb_*` tools without the user manually
//! running `claude mcp add`.
//!
//! The MCP server (`mcp/flock-mcp.mjs`) needs its npm deps. We can't rely on the
//! source tree's `node_modules` (a fresh worktree has none) or on a global
//! install, so we copy the server into Flock's data dir and `npm install` there
//! once. The orchestrator's project `.mcp.json` then points at this stable path.
//!
//! Everything here is best-effort: if the source can't be found or npm isn't
//! available, we return None and the orchestrator is still created — it just
//! won't have the Flock MCP pre-wired (the system prompt notes that).

use crate::error::AppResult;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// The files that make up the MCP server (everything but `node_modules`, which
/// is produced by `npm install`).
const MCP_FILES: &[&str] = &["flock-mcp.mjs", "package.json", "package-lock.json"];

fn data_mcp_dir() -> AppResult<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| crate::error::AppError::msg("no data local dir"))?
        .join("Flock")
        .join("mcp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Locate the source `mcp/` directory: the bundled resource in a packaged .app,
/// or the repo's `mcp/` next to `src-tauri/` in dev.
fn source_mcp_dir(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(res) = app.path().resource_dir() {
        let candidate = res.join("mcp");
        if candidate.join("flock-mcp.mjs").exists() {
            return Some(candidate);
        }
    }
    // Dev: CARGO_MANIFEST_DIR is .../src-tauri; the server lives in ../mcp.
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.join("mcp");
    if dev.join("flock-mcp.mjs").exists() {
        return Some(dev);
    }
    None
}

/// Ensure a runnable copy of the Flock MCP server exists in the data dir and
/// return the absolute path to `flock-mcp.mjs`. Copies the server files (cheap)
/// and runs `npm install` once if `node_modules` is missing. Returns None if the
/// source can't be found or the install fails.
pub fn ensure_installed(app: &AppHandle) -> Option<PathBuf> {
    let src = source_mcp_dir(app)?;
    let dst = data_mcp_dir().ok()?;

    for f in MCP_FILES {
        let from = src.join(f);
        if from.exists() {
            let _ = std::fs::copy(&from, dst.join(f));
        }
    }

    let entry = dst.join("flock-mcp.mjs");
    if !entry.exists() {
        return None;
    }

    if !dst.join("node_modules").join("@modelcontextprotocol").exists() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let cmd = format!(
            "cd {} && npm install --omit=dev --no-audit --no-fund 2>&1",
            shell_quote(&dst.to_string_lossy()),
        );
        // Login shell so npm is on PATH past the GUI's minimal env. Blocking and
        // potentially slow (a few seconds) — callers run this off the UI thread.
        let out = std::process::Command::new(shell)
            .args(["-i", "-l", "-c", &cmd])
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                eprintln!(
                    "flock: mcp npm install failed: {}",
                    String::from_utf8_lossy(&o.stdout).trim()
                );
                return None;
            }
            Err(e) => {
                eprintln!("flock: mcp npm install could not run: {e}");
                return None;
            }
        }
    }
    Some(entry)
}

/// Minimal single-quote shell escaping for the one path we interpolate.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Install the MCP server in the background at app startup, so the first
/// orchestrator creation doesn't pay the one-time `npm install` cost (which
/// would otherwise block the create call). Best-effort and idempotent.
pub fn prewarm(app: AppHandle) {
    std::thread::spawn(move || {
        if ensure_installed(&app).is_some() {
            eprintln!("flock: MCP server ready for orchestrators");
        }
    });
}
