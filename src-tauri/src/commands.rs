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
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
use tauri::{AppHandle, Emitter, State};

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
                "worktree",
                None,
            );
        }
    }
    Ok(repo)
}

#[tauri::command]
pub fn repos_list(state: State<'_, AppState>) -> AppResult<Vec<Repo>> {
    Ok(state
        .db
        .list_repos()?
        .into_iter()
        .filter(|r| !is_internal_repo(r))
        .collect())
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
    /// "worktree" (default) or "orchestrator". Set internally; the frontend
    /// never sends it for normal creates.
    #[serde(default)]
    pub kind: Option<String>,
    /// The orchestrator that spawned this worktree, if any. Set by the
    /// orchestration path so the fleet can be reconstructed.
    #[serde(default)]
    pub parent_id: Option<i64>,
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
                let _ = git::fast_forward_local_branch(&repo_path, &default);
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
        args.kind.as_deref().unwrap_or("worktree"),
        args.parent_id,
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

/// Tear down a single worktree: kill its tmux session + PTY client, drop its
/// resume-on-input lock, remove its git worktree (or scratch dir, for an
/// orchestrator), and delete its DB row. `force` is passed to
/// `git::remove_worktree` so callers can delete even dirty/unpushed trees.
fn teardown_worktree(state: &State<'_, AppState>, w: &Worktree, force: bool) -> AppResult<()> {
    // Tear down the tmux session and the PTY client before removing the
    // worktree directory, otherwise tmux's pane cwd points at a vanishing dir
    // and the server logs get noisy.
    state.pty.kill(w.id).ok();
    pty::tmux_kill_session(w.id);
    // Drop the worktree's resume-on-input lock — it's gone for good now.
    state.input_locks.lock().unwrap().remove(&w.id);
    if w.kind == "orchestrator" {
        // An orchestrator isn't a git worktree — it's a plain scratch dir. Just
        // remove the directory.
        let _ = std::fs::remove_dir_all(Path::new(&w.path));
    } else {
        let repo = state.db.get_repo(w.repo_id)?;
        let _ = git::remove_worktree(Path::new(&repo.path), Path::new(&w.path), force);
    }
    state.db.delete_worktree(w.id)?;
    Ok(())
}

#[tauri::command]
pub fn worktree_remove(
    state: State<'_, AppState>,
    id: i64,
    force: bool,
) -> AppResult<()> {
    let w = state.db.get_worktree(id)?;
    // Removing an orchestrator cascades to its fleet: every child worktree gets
    // the full teardown first. We force-remove children (deleting even
    // uncommitted/unpushed work) per the product decision — the sidebar warns
    // about this before confirming. The DB's ON DELETE SET NULL stays as a
    // safety net, but this explicit loop is what actually cleans up the child
    // git worktrees and tmux sessions, which a SQL cascade alone can't do.
    if w.kind == "orchestrator" {
        for child in state.db.list_children(id)? {
            teardown_worktree(&state, &child, true)?;
        }
    }
    teardown_worktree(&state, &w, force)
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

/// PR lifecycle status for one worktree, computed on demand (the background
/// poller in `pr.rs` keeps it fresh after this initial paint). None = no badge.
#[tauri::command]
pub fn worktree_refresh_pr_status(
    state: State<'_, AppState>,
    id: i64,
) -> AppResult<Option<crate::pr::PrStatus>> {
    let w = state.db.get_worktree(id)?;
    let repo = state.db.get_repo(w.repo_id)?;
    let default_branch = git::detect_default_branch(Path::new(&repo.path))
        .unwrap_or_else(|_| "main".to_string());
    Ok(crate::pr::compute(Path::new(&w.path), &default_branch))
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
#[allow(clippy::too_many_arguments)]
pub fn start_task_core(
    app: &AppHandle,
    state: &AppState,
    repo_id: i64,
    prompt: &str,
    branch: Option<String>,
    base: Option<String>,
    title: Option<String>,
    permission_mode: Option<String>,
    parent_id: Option<i64>,
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
            kind: None,
            parent_id,
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
        None,
        None,
    )?;
    state.db.touch_worktree(w.id)?;
    // Tell the desktop UI a worktree appeared so it shows up live (under its
    // repo, and — if parent_id is set — in the spawning orchestrator's fleet)
    // without waiting for a manual refresh. Mirrors the other worktree:* events.
    let _ = app.emit("worktree:created", &w);
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

// ---------- Orchestrator sessions ----------
//
// An orchestrator is a repo-less Claude session that directs a fleet of agents
// across many repos. It's modeled as a worktree row (kind='orchestrator') so it
// inherits the whole session stack — monitor, titles, transcript, PWA, input,
// hibernation — for free. It lives in a Flock-managed scratch dir, owned by an
// internal "Orchestrators" repo that exists only to satisfy the repo_id FK and
// is hidden from the normal repo list.

const ORCHESTRATOR_REPO_NAME: &str = "Orchestrators";

/// The scratch area where orchestrator sessions live (one subdir each). A plain
/// directory — not a git repo.
fn orchestrators_root() -> AppResult<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| AppError::msg("no data local dir"))?
        .join("Flock")
        .join("orchestrators");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get-or-create the internal repo that owns orchestrator sessions.
fn ensure_internal_repo(db: &Db) -> AppResult<Repo> {
    let root = orchestrators_root()?;
    db.insert_repo(ORCHESTRATOR_REPO_NAME, root.to_string_lossy().as_ref())
}

/// True for the internal orchestrators repo (matched by its on-disk path), so
/// the frontend repo list can hide it.
fn is_internal_repo(repo: &Repo) -> bool {
    orchestrators_root()
        .map(|root| Path::new(&repo.path) == root)
        .unwrap_or(false)
}

/// Write a project-local `.mcp.json` wiring the Flock MCP into the orchestrator.
/// No token is stored here — the MCP server reads it from the data dir itself.
fn write_orchestrator_mcp_config(dir: &Path, mjs: &Path) {
    let cfg = serde_json::json!({
        "mcpServers": {
            "flock": {
                "command": "node",
                "args": [mjs.to_string_lossy()],
            }
        }
    });
    let body = serde_json::to_string_pretty(&cfg).unwrap_or_default();
    if let Err(e) = std::fs::write(dir.join(".mcp.json"), body) {
        eprintln!("flock: write .mcp.json for orchestrator: {e}");
    }
}

/// The orchestrator's appended system prompt: what it is, the repos it can spawn
/// into, and how to drive + watch its fleet via the Flock MCP tools.
fn orchestrator_system_prompt(repos: &[Repo], has_mcp: bool) -> String {
    let repo_list = if repos.is_empty() {
        "(none registered yet — ask the user to add repos in Flock)".to_string()
    } else {
        repos
            .iter()
            .map(|r| format!("- {}", r.name))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let tools = if has_mcp {
        "You have the Flock MCP tools:\n\
         - task_create(repo, prompt): spawn an agent in a fresh worktree of `repo`. The `prompt` is delivered as the agent's FIRST TURN and runs automatically — put the full, self-contained task instructions HERE. It appears in Flock's UI and is linked to you as a child (your fleet).\n\
         - task_list / task_status: see your whole fleet and whose turn it is (working / idle / needs_input).\n\
         - task_read(id): read a child agent's conversation transcript so you can follow its work.\n\
         - task_input(id, text, submit): send a FOLLOW-UP to a running child (answer a question, redirect, unblock). To send a message it will act on, pass submit:true — that types the text AND presses Enter. Plain text without submit just sits in its input box UNSENT. Do NOT use task_input to give a child its initial task — use task_create's prompt for that.\n\
         - kb_search / kb_read / kb_ingest: your durable memory across sessions."
    } else {
        "The Flock MCP tools could not be auto-wired. Ask the user to enable Remote access in Flock settings and add the Flock MCP, then restart you."
    };
    format!(
        "You are an ORCHESTRATOR session in Flock. You don't ship code yourself — \
you direct a fleet of Claude agents, each working in its own git worktree/branch \
in a real repo. You run in a scratch directory, so use it freely for plans and \
notes, but the actual code changes happen in the agents you spawn.\n\n\
Registered repos you can spawn agents into:\n{repo_list}\n\n{tools}\n\n\
How to work: break the user's goal into per-repo tasks, spawn agents with \
task_create (in parallel when independent), follow their progress with task_read, \
and unblock any that need input with task_input. Give each agent a crisp, \
self-contained prompt; it can't see this conversation.\n\n\
Following your fleet: you are NOT notified when a child changes state — Flock \
doesn't ping you. When you want to know where a child stands, check it yourself with \
task_status (the whole fleet's states) or task_read (one child's transcript). A \
child you spawned keeps running on its own whether or not you're watching, so do \
this at natural checkpoints — not in a loop. Do NOT sit in a self-scheduled timer \
re-reading children that haven't moved; that just burns tokens. To unblock or \
redirect a child — including one that's gone idle or whose session has died — use \
task_input (submit:true); Flock resumes a dead child transparently and delivers your \
message."
    )
}

/// Create an orchestrator session: a repo-less scratch dir with the Flock MCP
/// auto-wired and an orchestration system prompt. Shared by the command and any
/// future headless caller.
pub fn start_orchestrator_core(
    app: &AppHandle,
    state: &AppState,
    prompt: &str,
    title: Option<String>,
    permission_mode: Option<String>,
    env: Option<String>,
) -> AppResult<Worktree> {
    let repo = ensure_internal_repo(&state.db)?;
    let root = orchestrators_root()?;

    // A unique scratch dir, slugged from the prompt.
    let leaf = branch_from_prompt(prompt);
    let mut path = root.join(&leaf);
    let mut n = 2;
    while path.exists() {
        path = root.join(format!("{leaf}-{n}"));
        n += 1;
    }
    std::fs::create_dir_all(&path)?;

    // .claude/settings.local.json with enableAllProjectMcpServers so the project
    // .mcp.json loads without a prompt.
    bootstrap_claude_settings(Path::new(&repo.path), &path);

    // Best-effort: install + wire the Flock MCP so the orchestrator can spawn
    // and watch agents out of the box.
    let mcp_path = crate::mcp::ensure_installed(app);
    if let Some(mjs) = &mcp_path {
        write_orchestrator_mcp_config(&path, mjs);
    }

    let pm = permission_mode.as_deref().unwrap_or(DEFAULT_PERMISSION_MODE);
    validate_permission_mode(pm)?;

    let w = state.db.insert_worktree(
        repo.id,
        &leaf,
        path.to_string_lossy().as_ref(),
        title.as_deref(),
        pm,
        "orchestrator",
        None,
    )?;

    // The MCP talks to the REST API — make sure it's running.
    let _ = crate::api::remote_start(app.clone());

    let repos: Vec<Repo> = state
        .db
        .list_repos()?
        .into_iter()
        .filter(|r| !is_internal_repo(r))
        .collect();
    let sys = orchestrator_system_prompt(&repos, mcp_path.is_some());

    // An orchestrator has no repo path to match a binding on, so honor its
    // explicitly chosen profile; fall back to path-based resolution otherwise.
    let cfg = env_profiles::load();
    let env_vars = match env.as_deref() {
        Some(name) => env_profiles::resolve_vars_by_name(&cfg, Some(name)),
        None => env_profiles::resolve_vars(&cfg, &path.to_string_lossy()),
    };
    pty::start_detached(w.id, &path, pm, &env_vars, Some(prompt), Some(&sys), None)?;
    state.db.touch_worktree(w.id)?;
    let _ = app.emit("worktree:created", &w);
    Ok(w)
}

#[derive(Deserialize)]
pub struct CreateOrchestratorArgs {
    pub prompt: String,
    pub title: Option<String>,
    pub permission_mode: Option<String>,
    /// Name of the env profile to run under. None resolves by scratch path
    /// (i.e. the default account).
    pub env: Option<String>,
}

/// Spawn an orchestrator session from the desktop. Returns the new worktree so
/// the UI can open it.
#[tauri::command]
pub fn orchestrator_create(
    app: AppHandle,
    state: State<'_, AppState>,
    args: CreateOrchestratorArgs,
) -> AppResult<Worktree> {
    start_orchestrator_core(
        &app,
        &state,
        &args.prompt,
        args.title,
        args.permission_mode,
        args.env,
    )
}

/// Every orchestrator session (kind='orchestrator'), across the internal repo.
#[tauri::command]
pub fn orchestrators_list(state: State<'_, AppState>) -> AppResult<Vec<Worktree>> {
    Ok(state
        .db
        .list_all_worktrees()?
        .into_iter()
        .filter(|w| w.kind == "orchestrator")
        .collect())
}

/// Spawn a task (worktree + prompted claude) from the desktop. Returns the new
/// worktree so the UI can open it.
#[tauri::command]
pub fn task_create(
    app: AppHandle,
    state: State<'_, AppState>,
    args: CreateTaskArgs,
) -> AppResult<Worktree> {
    start_task_core(
        &app,
        &state,
        args.repo_id,
        &args.prompt,
        args.branch,
        args.base,
        args.title,
        args.permission_mode,
        None,
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
/// The conversation itself survives: `session_open` reattaches with
/// `claude --resume <id>` (the killed session is gone, so the resume-on-
/// reattach path fires), so the workspace comes back with its history intact
/// under the new permission mode.
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

/// How long to wait for a resumed session's Claude TUI to be ready before
/// delivering input. Generous — a cold `claude --resume` reloads global config
/// plus the transcript. On timeout we send anyway (the session is live), so this
/// only ever delays a delivery, never fails it.
const RESUME_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Why delivering input couldn't reach the agent. Each variant maps to a clear
/// outcome — never an opaque 502 (see `api::input_error_response`).
#[derive(Debug)]
pub enum DeliverError {
    /// No worktree row for this id.
    NotFound,
    /// Worktree exists but has no persisted Claude session to resume.
    NoResumable,
    /// Resuming the session (tmux spawn) failed.
    Spawn(String),
    /// Session is live but tmux refused the keystroke — genuinely unexpected.
    SendFailed,
}

/// Deliver keystrokes to a worktree's Claude session, resuming the persisted
/// session from disk (`claude --resume`) if its tmux session has died (monitor
/// hibernation, memory reaping, reboot). Blocking: tmux calls plus a readiness
/// poll up to `RESUME_READY_TIMEOUT`. A per-worktree lock serializes concurrent
/// callers so a dead session is resumed exactly once (the second waits, then
/// finds it live). Shared by the REST input handler and the monitor's
/// parent-wake — **call from a blocking context** (spawn_blocking or a dedicated
/// thread), never the monitor poll loop.
///
/// `literal` types `payload` verbatim; otherwise `payload` is a tmux key name.
/// `submit` presses Enter after literal text (a small gap lets the TUI ingest
/// the text first), turning the input into a submitted turn.
pub fn deliver_input(
    state: &AppState,
    id: i64,
    literal: bool,
    payload: &str,
    submit: bool,
) -> Result<(), DeliverError> {
    // Per-worktree lock so concurrent deliveries to a dead session resume once.
    let lock = {
        let mut locks = state.input_locks.lock().unwrap();
        locks
            .entry(id)
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().unwrap();

    // Resume the persisted session if tmux has none live for this worktree. The
    // live path is untouched — no resume, no readiness wait.
    if !pty::tmux_list_sessions().contains(&id) {
        let w = state.db.get_worktree(id).map_err(|_| DeliverError::NotFound)?;
        let cwd = Path::new(&w.path);
        // Env-profile vars by the repo's path, mirroring `session_open`. Resolved
        // first so the resume lookup uses the session's own CLAUDE_CONFIG_DIR.
        let env_vars = match state.db.get_repo(w.repo_id) {
            Ok(repo) => env_profiles::resolve_vars(&env_profiles::load(), &repo.path),
            Err(_) => Vec::new(),
        };
        let resume_id =
            pty::latest_session_id(cwd, crate::transcript::config_dir_from_env(&env_vars))
                .ok_or(DeliverError::NoResumable)?;
        // Headless resume — no PTY client (no viewer), no `--append-system-prompt`
        // (mirrors the desktop reattach; `--resume` restores the conversation).
        pty::start_detached(id, cwd, &w.permission_mode, &env_vars, None, None, Some(&resume_id))
            .map_err(|e| DeliverError::Spawn(e.to_string()))?;
        // Wait for Claude's input UI before typing; on timeout, send anyway.
        pty::wait_until_ready(id, RESUME_READY_TIMEOUT);
    }

    let sent = pty::tmux_send(id, literal, payload);
    let sent = if sent && submit {
        std::thread::sleep(Duration::from_millis(120));
        pty::tmux_send(id, false, "Enter")
    } else {
        sent
    };
    if sent {
        Ok(())
    } else {
        Err(DeliverError::SendFailed)
    }
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

/// Tell the backend which worktree pane is focused. The idle-hibernation
/// monitor reads this so it never reaps the session you're actively looking at.
/// `None` clears it (no pane focused).
#[tauri::command]
pub fn set_active_worktree(state: State<'_, AppState>, worktree_id: Option<i64>) {
    *state.active_worktree.lock().unwrap() = worktree_id;
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
pub fn schedule_run_now(
    app: AppHandle,
    state: State<'_, AppState>,
    id: i64,
) -> AppResult<Worktree> {
    let s = state.db.get_schedule(id)?;
    let title = s
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| Some(format!("scheduled: {}", s.spec)));
    let w = start_task_core(&app, &state, s.repo_id, &s.prompt, None, None, title, None, None)?;
    if let Some(spec) = schedule::parse_spec(&s.spec) {
        let now = now_unix();
        let _ = state
            .db
            .mark_schedule_run(id, now, schedule::next_run(&spec, now));
    }
    Ok(w)
}

// ---------- Knowledge base ----------

/// The currently configured Obsidian vault path, or None if unset.
#[tauri::command]
pub fn kb_get_vault() -> Option<String> {
    crate::kb::vault_path()
}

/// Point the knowledge base at a vault folder (created if missing), persist it,
/// run an initial index, and (re)start the live watcher. Returns the number of
/// notes indexed.
#[tauri::command]
pub fn kb_set_vault(app: AppHandle, state: State<'_, AppState>, path: String) -> AppResult<usize> {
    let vault = path.trim().to_string();
    if vault.is_empty() {
        return Err(AppError::msg("empty vault path"));
    }
    std::fs::create_dir_all(&vault)?;
    crate::kb::save_config(&crate::kb::KbConfig {
        vault_path: Some(vault.clone()),
    })
    .map_err(|e| AppError::msg(e.to_string()))?;
    let count = crate::kb::reindex(&state.db, &vault)?;
    crate::kb::restart_watcher(&app, Some(vault));
    Ok(count)
}

/// Re-scan the configured vault. Returns the number of notes (re)indexed.
#[tauri::command]
pub fn kb_reindex(state: State<'_, AppState>) -> AppResult<usize> {
    match crate::kb::vault_path() {
        Some(v) => crate::kb::reindex(&state.db, &v),
        None => Ok(0),
    }
}

/// Full-text search the knowledge base (for a future desktop search UI; the
/// agent-facing path is the MCP `kb_search` tool → REST).
#[tauri::command]
pub fn kb_search(
    state: State<'_, AppState>,
    query: String,
    limit: Option<i64>,
) -> AppResult<Vec<crate::db::KbHit>> {
    let q = crate::kb::sanitize_query(&query);
    if q.is_empty() {
        return Ok(Vec::new());
    }
    state.db.kb_search(&q, limit.unwrap_or(20))
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
