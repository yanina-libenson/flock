use crate::db::{Db, Repo, Schedule, Worktree, DEFAULT_PERMISSION_MODE};
use crate::env_profiles;
use crate::error::{AppError, AppResult};
use crate::git;
use crate::pty;
use crate::schedule;
use crate::state::AppState;
use base64::Engine;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
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
            let _ = state.db.insert_worktree(
                repo.id,
                &branch,
                &e.path,
                None,
                DEFAULT_PERMISSION_MODE,
            );
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
    /// Per-worktree Claude permission mode. Omit / None falls back to
    /// `DEFAULT_PERMISSION_MODE` ("bypassPermissions"). Validated server-side
    /// against the same whitelist as `worktree_set_permission_mode`.
    pub permission_mode: Option<String>,
}

/// Permission-mode values forwarded to `claude --permission-mode`.
/// Whitelisted in the backend because the value goes onto a shell command
/// line; anything outside this list is rejected.
const ALLOWED_PERMISSION_MODES: &[&str] = &[
    "default",
    "bypassPermissions",
    "acceptEdits",
    "auto",
    "dontAsk",
    "plan",
];

fn validate_permission_mode(mode: &str) -> AppResult<()> {
    if ALLOWED_PERMISSION_MODES.contains(&mode) {
        Ok(())
    } else {
        Err(AppError::msg(format!(
            "invalid permission_mode {mode:?}; must be one of {ALLOWED_PERMISSION_MODES:?}"
        )))
    }
}

#[tauri::command]
pub fn worktree_create(
    state: State<'_, AppState>,
    args: CreateWorktreeArgs,
) -> AppResult<Worktree> {
    create_worktree_core(&state.db, args)
}

/// Worktree-creation core, callable without a Tauri `State` wrapper so both the
/// command and the orchestration path (task_create / `POST /api/tasks`) share
/// one implementation.
fn create_worktree_core(db: &Db, args: CreateWorktreeArgs) -> AppResult<Worktree> {
    let repo = db.get_repo(args.repo_id)?;
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
        if let Err(e) = git::add_worktree(
            &repo_path,
            &path,
            &args.branch,
            effective_base.as_deref(),
            true,
        ) {
            eprintln!(
                "flock: worktree_create new_branch failed: repo={} branch={:?} base={:?} path={} err={}",
                repo_path.display(),
                args.branch,
                effective_base,
                path.display(),
                e
            );
            return Err(e);
        }
    } else {
        let _ = git::fetch_branch(&repo_path, "origin", &args.branch);
        if let Err(e) = git::add_worktree(&repo_path, &path, &args.branch, None, false) {
            eprintln!(
                "flock: worktree_create existing_branch failed: repo={} branch={:?} path={} err={}",
                repo_path.display(),
                args.branch,
                path.display(),
                e
            );
            return Err(e);
        }
    }
    bootstrap_claude_settings(&repo_path, &path);

    let permission_mode = args
        .permission_mode
        .as_deref()
        .unwrap_or(DEFAULT_PERMISSION_MODE);
    validate_permission_mode(permission_mode)?;

    let w = db.insert_worktree(
        repo.id,
        &args.branch,
        path.to_string_lossy().as_ref(),
        args.title.as_deref(),
        permission_mode,
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
    // Resolve per-environment vars by the *repo's* registered path (worktrees
    // live elsewhere, so folder bindings must key off the repo's location).
    let env_vars = match state.db.get_repo(w.repo_id) {
        Ok(repo) => env_profiles::resolve_vars(&env_profiles::load(), &repo.path),
        Err(_) => Vec::new(),
    };
    state.pty.attach(
        &app,
        args.worktree_id,
        Path::new(&w.path),
        args.cols,
        args.rows,
        &w.permission_mode,
        &env_vars,
        None,
    )?;
    state.db.touch_worktree(args.worktree_id)?;
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateTaskArgs {
    pub repo_id: i64,
    pub prompt: String,
    /// Branch leaf; auto-derived from the prompt when omitted.
    pub branch: Option<String>,
    pub base: Option<String>,
    pub title: Option<String>,
    pub permission_mode: Option<String>,
}

/// Orchestration primitive: create a worktree and start claude on it with an
/// initial prompt, headlessly (no viewer needed). This is the substrate for
/// loops — cron, MCP, the REST API, or another agent can spawn a task and walk
/// away; the monitor picks up its status and auto-titles it. Shared by the
/// `task_create` command and `POST /api/tasks`.
pub fn start_task_core(
    state: &AppState,
    repo_id: i64,
    prompt: &str,
    branch: Option<String>,
    base: Option<String>,
    title: Option<String>,
    permission_mode: Option<String>,
) -> AppResult<Worktree> {
    let leaf = branch.unwrap_or_else(|| branch_from_prompt(prompt));
    // Create the worktree, retrying with a numeric suffix on branch collision
    // (the loop caller can't know what names are already taken).
    let mut last_err = None;
    let mut w = None;
    for attempt in 0..6 {
        let candidate = if attempt == 0 {
            leaf.clone()
        } else {
            format!("{leaf}-{}", attempt + 1)
        };
        let full = if candidate.contains('/') {
            candidate
        } else {
            format!("flock/{candidate}")
        };
        let args = CreateWorktreeArgs {
            repo_id,
            branch: full,
            base: base.clone(),
            title: title.clone(),
            new_branch: true,
            path: None,
            permission_mode: permission_mode.clone(),
        };
        match create_worktree_core(&state.db, args) {
            Ok(created) => {
                w = Some(created);
                break;
            }
            Err(e) if is_branch_collision(&e) => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    let w = w.ok_or_else(|| {
        last_err.unwrap_or_else(|| AppError::msg("could not create worktree after retries"))
    })?;

    let repo = state.db.get_repo(repo_id)?;
    let env_vars = env_profiles::resolve_vars(&env_profiles::load(), &repo.path);
    pty::start_detached(
        w.id,
        Path::new(&w.path),
        &w.permission_mode,
        &env_vars,
        Some(prompt),
    )?;
    state.db.touch_worktree(w.id)?;
    Ok(w)
}

fn is_branch_collision(e: &AppError) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("already exists")
        || s.contains("already used by worktree")
        || s.contains("already checked out")
}

/// Derive a branch leaf from a prompt: first few words, slugified, capped.
fn branch_from_prompt(prompt: &str) -> String {
    let words = prompt
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    // Guard before slugify: slugify falls back to "branch" on empty input,
    // but "task" is a clearer default for a prompt-less task.
    if words.trim().is_empty() {
        return "task".to_string();
    }
    let slug: String = git::slugify(&words).chars().take(40).collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

/// Spawn a task (worktree + prompted claude) from the desktop. Returns the new
/// worktree so the UI can open it.
#[tauri::command]
pub fn task_create(state: State<'_, AppState>, args: CreateTaskArgs) -> AppResult<Worktree> {
    start_task_core(
        &state,
        args.repo_id,
        &args.prompt,
        args.branch,
        args.base,
        args.title,
        args.permission_mode,
    )
}

/// Read the full environments + folder-bindings config (tokens included; this
/// is the user's own machine and the desktop UI).
#[tauri::command]
pub fn env_config_get() -> env_profiles::EnvConfig {
    env_profiles::load()
}

/// Persist the environments + folder-bindings config (0600 in the data dir).
#[tauri::command]
pub fn env_config_set(config: env_profiles::EnvConfig) -> AppResult<()> {
    env_profiles::save(&config).map_err(|e| AppError::msg(e.to_string()))?;
    Ok(())
}

/// Update the persisted permission mode for a worktree. Tears down the live
/// tmux session so the next `session_open` restarts `claude` with the new
/// flag — without this the old `claude` keeps running with the old mode
/// (tmux `new-session -A` attaches to existing sessions verbatim).
///
/// Callers should warn the user that this discards the current Claude
/// conversation in that workspace.
#[tauri::command]
pub fn worktree_set_permission_mode(
    state: State<'_, AppState>,
    id: i64,
    mode: String,
) -> AppResult<()> {
    validate_permission_mode(&mode)?;
    state.db.update_worktree_permission_mode(id, &mode)?;
    state.pty.kill(id).ok();
    pty::tmux_kill_session(id);
    Ok(())
}

/// Reflow the worktree's tmux window to a size. The desktop calls this to
/// reclaim its full width when its pane becomes active (after the phone may
/// have narrowed the session). See `pty::tmux_resize_window`.
#[tauri::command]
pub fn worktree_resize_window(worktree_id: i64, cols: u16, rows: u16) -> AppResult<()> {
    pty::tmux_resize_window(worktree_id, cols, rows);
    Ok(())
}

/// Set a worktree's display title. Used to correct or override the title the
/// monitor auto-generates from the session. Display-only — branch and worktree
/// path stay locked to the original slug.
#[tauri::command]
pub fn worktree_set_title(state: State<'_, AppState>, id: i64, title: String) -> AppResult<()> {
    state.db.update_worktree_title(id, &title)?;
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

#[derive(Deserialize)]
pub struct CreateScheduleArgs {
    pub repo_id: i64,
    pub prompt: String,
    pub spec: String,
    pub title: Option<String>,
}

/// Create a scheduled task. Shared validation/insert used by the command and
/// `POST /api/schedules`.
pub fn schedule_create_core(
    db: &Db,
    repo_id: i64,
    prompt: &str,
    spec: &str,
    title: Option<&str>,
) -> AppResult<Schedule> {
    let parsed = schedule::parse_spec(spec)
        .ok_or_else(|| AppError::msg("invalid spec; use '@every 30m' or 'HH:MM'"))?;
    let next = schedule::initial_next_run(&parsed, now_unix());
    db.insert_schedule(repo_id, prompt, spec, title, next)
}

#[tauri::command]
pub fn schedule_create(state: State<'_, AppState>, args: CreateScheduleArgs) -> AppResult<Schedule> {
    schedule_create_core(
        &state.db,
        args.repo_id,
        &args.prompt,
        &args.spec,
        args.title.as_deref(),
    )
}

#[tauri::command]
pub fn schedule_list(state: State<'_, AppState>) -> AppResult<Vec<Schedule>> {
    state.db.list_schedules()
}

#[tauri::command]
pub fn schedule_set_enabled(state: State<'_, AppState>, id: i64, enabled: bool) -> AppResult<()> {
    state.db.set_schedule_enabled(id, enabled)
}

#[tauri::command]
pub fn schedule_delete(state: State<'_, AppState>, id: i64) -> AppResult<()> {
    state.db.delete_schedule(id)
}

/// Fire a schedule immediately, out of cycle, and roll its next_run forward so
/// the regular tick doesn't double-fire.
#[tauri::command]
pub fn schedule_run_now(state: State<'_, AppState>, id: i64) -> AppResult<Worktree> {
    let s = state.db.get_schedule(id)?;
    let title = s
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| Some(format!("scheduled: {}", s.spec)));
    let w = start_task_core(&state, s.repo_id, &s.prompt, None, None, title, None)?;
    if let Some(spec) = schedule::parse_spec(&s.spec) {
        let now = now_unix();
        let _ = state
            .db
            .mark_schedule_run(id, now, schedule::next_run(&spec, now));
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_from_prompt_is_a_sane_slug() {
        let b = branch_from_prompt("Fix the checkout race condition, please!");
        assert!(!b.is_empty());
        assert!(!b.contains(' '));
        assert!(b.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(b.len() <= 40);
        assert_eq!(branch_from_prompt("   "), "task");
        assert_eq!(branch_from_prompt(""), "task");
    }
}
