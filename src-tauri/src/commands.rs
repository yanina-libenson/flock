use crate::db::{Repo, Worktree};
use crate::error::{AppError, AppResult};
use crate::git;
use crate::pty;
use crate::state::AppState;
use base64::Engine;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, State};

// ---------- Repo commands ----------

#[tauri::command]
pub fn repo_add(state: State<'_, AppState>, path: String) -> AppResult<Repo> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(AppError::msg("path does not exist"));
    }
    let root = git::canonical_repo_root(&p)?;
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string();
    let repo = state.db.insert_repo(&name, root.to_string_lossy().as_ref())?;

    let entries = git::list_worktrees(&root).unwrap_or_default();
    for e in entries {
        if let Some(branch) = e.branch.clone() {
            let _ = state.db.insert_worktree(repo.id, &branch, &e.path, None);
        }
    }
    Ok(repo)
}

#[tauri::command]
pub fn repos_list(state: State<'_, AppState>) -> AppResult<Vec<Repo>> {
    state.db.list_repos()
}

#[tauri::command]
pub fn repo_remove(state: State<'_, AppState>, id: i64) -> AppResult<()> {
    // Best-effort: kill any live PTY attach and tmux session for each worktree
    // before the DB cascade deletes them. Worktree directories on disk stay
    // put — re-adding the repo re-discovers them via `git worktree list`.
    if let Ok(worktrees) = state.db.list_worktrees(id) {
        for w in worktrees {
            state.pty.kill(w.id).ok();
            pty::tmux_kill_session(w.id);
        }
    }
    state.db.delete_repo(id)
}

#[tauri::command]
pub fn repo_branches(state: State<'_, AppState>, id: i64) -> AppResult<Vec<String>> {
    let repo = state.db.get_repo(id)?;
    git::list_branches(Path::new(&repo.path))
}

#[tauri::command]
pub fn repo_default_branch(state: State<'_, AppState>, id: i64) -> AppResult<String> {
    let repo = state.db.get_repo(id)?;
    git::detect_default_branch(Path::new(&repo.path))
}

#[tauri::command]
pub fn repo_all_branches(state: State<'_, AppState>, id: i64) -> AppResult<Vec<String>> {
    let repo = state.db.get_repo(id)?;
    git::list_all_branches(Path::new(&repo.path))
}

// ---------- Worktree commands ----------

#[derive(Debug, Deserialize)]
pub struct CreateWorktreeArgs {
    pub repo_id: i64,
    pub branch: String,
    pub base: Option<String>,
    pub title: Option<String>,
    pub new_branch: bool,
    pub path: Option<String>,
}

#[tauri::command]
pub fn worktree_create(
    state: State<'_, AppState>,
    args: CreateWorktreeArgs,
) -> AppResult<Worktree> {
    let repo = state.db.get_repo(args.repo_id)?;
    let repo_path = PathBuf::from(&repo.path);
    let slug = git::slugify(&args.branch);
    let path = match args.path {
        Some(p) => PathBuf::from(p),
        None => git::default_worktree_path(&repo_path, &slug)?,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if args.new_branch {
        let effective_base: Option<String> = match args.base.as_deref() {
            None | Some("default") => {
                let default = git::detect_default_branch(&repo_path)?;
                let _ = git::fetch_branch(&repo_path, "origin", &default);
                Some(format!("origin/{default}"))
            }
            Some("HEAD") | Some("") => None,
            Some(other) => Some(other.to_string()),
        };
        git::add_worktree(
            &repo_path,
            &path,
            &args.branch,
            effective_base.as_deref(),
            true,
        )?;
    } else {
        let _ = git::fetch_branch(&repo_path, "origin", &args.branch);
        git::add_worktree(&repo_path, &path, &args.branch, None, false)?;
    }
    bootstrap_claude_settings(&repo_path, &path);

    let w = state.db.insert_worktree(
        repo.id,
        &args.branch,
        path.to_string_lossy().as_ref(),
        args.title.as_deref(),
    )?;
    Ok(w)
}

/// Carry the parent repo's `.claude/settings.local.json` (MCP approvals, bash
/// allowlist) into the fresh worktree so the first `claude` invocation doesn't
/// re-prompt for every already-approved server/command. Best-effort: failures
/// here shouldn't block worktree creation, but we surface them on stderr so
/// a silent permissions/disk problem doesn't look like a bug in Claude.
fn bootstrap_claude_settings(repo: &Path, worktree: &Path) {
    let dst_dir = worktree.join(".claude");
    if let Err(e) = std::fs::create_dir_all(&dst_dir) {
        eprintln!(
            "flock: bootstrap_claude_settings: mkdir {}: {}",
            dst_dir.display(),
            e
        );
        return;
    }
    let dst = dst_dir.join("settings.local.json");
    if dst.exists() {
        return;
    }
    let src = repo.join(".claude").join("settings.local.json");
    if src.exists() {
        if let Err(e) = std::fs::copy(&src, &dst) {
            eprintln!(
                "flock: bootstrap_claude_settings: copy {} → {}: {}",
                src.display(),
                dst.display(),
                e
            );
        }
    } else if let Err(e) = std::fs::write(&dst, r#"{"enableAllProjectMcpServers":true}"#) {
        eprintln!(
            "flock: bootstrap_claude_settings: write {}: {}",
            dst.display(),
            e
        );
    }
}

#[tauri::command]
pub fn worktrees_list(state: State<'_, AppState>, repo_id: i64) -> AppResult<Vec<Worktree>> {
    state.db.list_worktrees(repo_id)
}

#[tauri::command]
pub fn worktree_remove(
    state: State<'_, AppState>,
    id: i64,
    force: bool,
) -> AppResult<()> {
    let w = state.db.get_worktree(id)?;
    let repo = state.db.get_repo(w.repo_id)?;
    // Tear down the tmux session and the PTY client before removing the
    // worktree directory, otherwise tmux's pane cwd points at a vanishing dir
    // and the server logs get noisy.
    state.pty.kill(id).ok();
    pty::tmux_kill_session(id);
    let _ = git::remove_worktree(Path::new(&repo.path), Path::new(&w.path), force);
    state.db.delete_worktree(id)?;
    Ok(())
}

#[tauri::command]
pub fn worktree_dirty(state: State<'_, AppState>, id: i64) -> AppResult<git::DirtySummary> {
    let w = state.db.get_worktree(id)?;
    git::dirty_summary(Path::new(&w.path))
}

#[tauri::command]
pub fn worktree_current_branch(
    state: State<'_, AppState>,
    id: i64,
) -> AppResult<String> {
    let w = state.db.get_worktree(id)?;
    git::current_branch(Path::new(&w.path))
}

// ---------- Session / PTY commands ----------
//
// "Session" here is a loose term — the real session lives inside tmux.
// These commands manage the PTY *client* that connects xterm to the tmux
// session for a given worktree.

#[derive(Debug, Deserialize)]
pub struct OpenSessionArgs {
    pub worktree_id: i64,
    pub cols: u16,
    pub rows: u16,
}

#[tauri::command]
pub fn session_open(
    app: AppHandle,
    state: State<'_, AppState>,
    args: OpenSessionArgs,
) -> AppResult<()> {
    let w = state.db.get_worktree(args.worktree_id)?;
    state.pty.attach(
        &app,
        args.worktree_id,
        Path::new(&w.path),
        args.cols,
        args.rows,
    )?;
    state.db.touch_worktree(args.worktree_id)?;
    Ok(())
}

#[tauri::command]
pub fn session_write(
    state: State<'_, AppState>,
    worktree_id: i64,
    b64: String,
) -> AppResult<()> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| AppError::msg(format!("bad base64: {e}")))?;
    state.pty.write(worktree_id, &bytes)
}

#[tauri::command]
pub fn session_resize(
    state: State<'_, AppState>,
    worktree_id: i64,
    cols: u16,
    rows: u16,
) -> AppResult<()> {
    state.pty.resize(worktree_id, cols, rows)
}

/// Tear down the PTY *client* for a worktree without killing the tmux session.
/// Called when the user closes a pane — the tmux session (and the `claude`
/// process inside it) keeps running, so reopening the pane reattaches.
#[tauri::command]
pub fn session_close(state: State<'_, AppState>, worktree_id: i64) -> AppResult<()> {
    state.pty.kill(worktree_id)
}

#[tauri::command]
pub fn tmux_check() -> bool {
    pty::tmux_available()
}
