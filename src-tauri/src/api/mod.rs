//! Local REST API, opt-in via the desktop Settings toggle (and auto-started for
//! orchestrators). It's the surface the Flock MCP server (`mcp/flock-mcp.mjs`)
//! talks to. Security posture: binds `127.0.0.1` only. All `/api/*` routes
//! require the master token (Bearer header).

use crate::db::Schedule;
use crate::monitor::WorktreeStatus;
use crate::state::AppState;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tauri::{AppHandle, Manager};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Flock's API port. Distinct from argus's 7743 so both can run side by side.
const PORT: u16 = 7765;

#[derive(Clone)]
struct ApiCtx {
    app: AppHandle,
    token: Arc<String>,
}

#[derive(Serialize)]
pub struct RemoteInfo {
    pub running: bool,
    pub token: String,
}

#[derive(Serialize)]
struct WorktreeRow {
    id: i64,
    repo: String,
    branch: String,
    title: Option<String>,
    status: Option<WorktreeStatus>,
    has_session: bool,
    model: Option<String>,
    effort: Option<String>,
    /// `"claude"` or `"codex"` — which agent runs this worktree's session.
    agent: String,
}

#[derive(Serialize, Default)]
struct StatusCounts {
    working: usize,
    idle: usize,
    needs_input: usize,
}

// ---------- token ----------

fn flock_dir() -> std::io::Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| std::io::Error::other("no data local dir"))?
        .join("Flock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn token_path() -> std::io::Result<PathBuf> {
    Ok(flock_dir()?.join("api-token"))
}

/// Read the master token, generating + persisting one (0600) on first use.
/// Stored in the data dir, never in the repo.
fn load_or_create_token() -> std::io::Result<String> {
    let path = token_path()?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let token = gen_token();
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(token)
}

fn gen_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn token_matches(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    a.len() == b.len() && a.ct_eq(b).into()
}

// ---------- auth ----------

fn extract_token(req: &Request) -> Option<String> {
    if let Some(h) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(s) = h.to_str() {
            if let Some(t) = s.strip_prefix("Bearer ") {
                return Some(t.trim().to_string());
            }
        }
    }
    None
}

async fn require_auth(State(ctx): State<ApiCtx>, req: Request, next: Next) -> Response {
    match extract_token(&req) {
        Some(t) if token_matches(&t, &ctx.token) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}

// ---------- handlers ----------

async fn worktrees(State(ctx): State<ApiCtx>) -> Json<Vec<WorktreeRow>> {
    let st = ctx.app.state::<AppState>();
    let statuses = st.statuses.lock().unwrap().clone();
    let mut out = Vec::new();
    if let Ok(repos) = st.db.list_repos() {
        for repo in repos {
            if let Ok(wts) = st.db.list_worktrees(repo.id) {
                for w in wts {
                    let status = statuses.get(&w.id).copied();
                    out.push(WorktreeRow {
                        id: w.id,
                        repo: repo.name.clone(),
                        branch: w.branch,
                        title: w.title,
                        status,
                        has_session: status.is_some(),
                        model: w.model,
                        effort: w.effort,
                        agent: w.agent,
                    });
                }
            }
        }
    }
    Json(out)
}

async fn status_counts(State(ctx): State<ApiCtx>) -> Json<StatusCounts> {
    let st = ctx.app.state::<AppState>();
    let mut c = StatusCounts::default();
    for status in st.statuses.lock().unwrap().values() {
        match status {
            WorktreeStatus::Working => c.working += 1,
            WorktreeStatus::Idle => c.idle += 1,
            WorktreeStatus::NeedsInput => c.needs_input += 1,
        }
    }
    Json(c)
}

#[derive(Deserialize)]
struct InputBody {
    text: Option<String>,
    key: Option<String>,
    /// When sending `text`, also press Enter to submit it as a turn. Without
    /// this, text is typed into the composer but left unsent (so callers can
    /// build up input or follow with a special key). Ignored for `key`.
    #[serde(default)]
    submit: Option<bool>,
}

/// Map a frontend key name to a tmux key token. Allowlisted — an unknown key
/// is rejected rather than forwarded.
fn map_key(key: &str) -> Option<&'static str> {
    Some(match key.to_ascii_lowercase().as_str() {
        "enter" => "Enter",
        "escape" | "esc" => "Escape",
        "tab" => "Tab",
        "shift-tab" | "btab" => "BTab",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "backspace" => "BSpace",
        "ctrl-c" => "C-c",
        "ctrl-d" => "C-d",
        "ctrl-u" => "C-u",
        _ => return None,
    })
}

/// Send input to a session: `{"text": "..."}` types literally, `{"key":"esc"}`
/// sends a special key. The agent's reply shows up on the SSE stream.
///
/// Resilient to a dead session: if the worktree's tmux session is gone
/// (hibernated by the monitor, reaped under memory pressure, lost to a reboot),
/// it is resumed transparently from Claude Code's on-disk transcript
/// (`claude --resume`) and the input is delivered once the session is ready —
/// so sending input "just works" whether the session was alive, idle, or dead.
/// The resume-aware delivery (and its per-worktree lock) lives in
/// `commands::deliver_input`, shared with children's notify_orchestrator.
async fn input(
    State(ctx): State<ApiCtx>,
    Path(id): Path<i64>,
    Json(body): Json<InputBody>,
) -> Response {
    let (literal, payload) = if let Some(text) = body.text {
        (true, text)
    } else if let Some(key) = body.key {
        match map_key(&key) {
            Some(tok) => (false, tok.to_string()),
            None => return (StatusCode::BAD_REQUEST, "unknown key").into_response(),
        }
    } else {
        return (StatusCode::BAD_REQUEST, "missing text or key").into_response();
    };
    // Only literal text can be auto-submitted; a bare key is already its own
    // action.
    let submit = literal && body.submit.unwrap_or(false);

    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        crate::commands::deliver_input(&st, id, literal, &payload, submit)
    })
    .await;

    match res {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => input_error_response(e, id),
        Err(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "input task join failed").into_response()
        }
    }
}

#[derive(Deserialize)]
struct NotifyBody {
    /// The reporting worktree — the MCP sends its own FLOCK_WORKTREE_ID. Its
    /// orchestrator is looked up from the row, never taken from the caller.
    from: i64,
    kind: String,
    text: String,
}

/// `notify_orchestrator`: a child messages the orchestrator that spawned it.
/// `{"from": id, "kind": "done|blocked|question|info", "text": "..."}`.
async fn notify_h(State(ctx): State<ApiCtx>, Json(body): Json<NotifyBody>) -> Response {
    let from = body.from;
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        crate::notify::notify_parent(&st, from, &body.kind, &body.text)
    })
    .await;
    match res {
        Ok(Ok(())) => Json(serde_json::json!({ "result": "delivered" })).into_response(),
        Ok(Err(e)) => notify_error_response(e, from),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "notify task join failed").into_response(),
    }
}

fn notify_error_response(err: crate::notify::NotifyError, id: i64) -> Response {
    use crate::notify::NotifyError;
    match err {
        NotifyError::NotFound => {
            (StatusCode::NOT_FOUND, format!("worktree {id} not found")).into_response()
        }
        NotifyError::NoParent => (
            StatusCode::CONFLICT,
            format!(
                "worktree {id} has no orchestrator — it wasn't spawned by one, so there's \
                 nobody to notify. Tell the user directly instead."
            ),
        )
            .into_response(),
        NotifyError::Invalid(e) => (StatusCode::BAD_REQUEST, e).into_response(),
        NotifyError::Deliver(e) => input_error_response(e, id),
    }
}

/// Map a `DeliverError` to a clear HTTP response. A dead, unknown, or
/// unresumable session yields 404 / 409 / 500 with a message — never an opaque
/// 502 (the pre-fix failure mode that made callers think the API was down).
#[derive(Deserialize, Default)]
struct RemoveWorktreeBody {
    #[serde(default)]
    force: bool,
    /// The calling orchestrator's worktree id, shown in the confirm dialog.
    #[serde(default)]
    requested_by: Option<i64>,
}

/// Remove a worktree (the sidebar ✕), for orchestrators, once the user approves
/// it in the desktop dialog. `{"force?": bool, "requested_by?": id}`. Holds the
/// request open until the user answers or `REMOVE_CONFIRM_TIMEOUT` passes.
async fn remove_worktree_h(
    State(ctx): State<ApiCtx>,
    Path(id): Path<i64>,
    body: Option<Json<RemoveWorktreeBody>>,
) -> Response {
    let RemoveWorktreeBody { force, requested_by } = body.map(|b| b.0).unwrap_or_default();
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        crate::commands::remove_worktree_for_orchestrator(&app, &st, id, force, requested_by)
    })
    .await;
    match res {
        Ok(Ok(())) => Json(serde_json::json!({ "id": id, "result": "removed" })).into_response(),
        Ok(Err(e)) => remove_refusal_response(e, id),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

fn remove_refusal_response(err: crate::commands::RemoveRefusal, id: i64) -> Response {
    use crate::commands::RemoveRefusal;
    match err {
        RemoveRefusal::NotFound => {
            (StatusCode::NOT_FOUND, format!("worktree {id} not found")).into_response()
        }
        RemoveRefusal::IsOrchestrator => (
            StatusCode::FORBIDDEN,
            format!("worktree {id} is an orchestrator — orchestrators can only be removed from the Flock UI"),
        )
            .into_response(),
        RemoveRefusal::Dirty(d) => (
            StatusCode::CONFLICT,
            format!(
                "worktree {id} has uncommitted changes ({} staged, {} unstaged, {} untracked). \
                 Tell the user what would be lost and only retry with force:true if they explicitly agree.",
                d.staged, d.unstaged, d.untracked
            ),
        )
            .into_response(),
        RemoveRefusal::Declined => (
            StatusCode::FORBIDDEN,
            format!(
                "the user declined removing worktree {id} in Flock, so it was not removed. \
                 Leave it in place; only try again if the user asks you to."
            ),
        )
            .into_response(),
        RemoveRefusal::TimedOut => (
            StatusCode::REQUEST_TIMEOUT,
            format!(
                "worktree {id} was not removed: the user didn't answer Flock's confirm dialog within {}s \
                 (treated as declined). Ask the user in chat before trying again.",
                crate::commands::REMOVE_CONFIRM_TIMEOUT.as_secs()
            ),
        )
            .into_response(),
        RemoveRefusal::Other(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

fn input_error_response(err: crate::commands::DeliverError, id: i64) -> Response {
    use crate::commands::DeliverError;
    match err {
        DeliverError::NotFound => {
            (StatusCode::NOT_FOUND, format!("worktree {id} not found")).into_response()
        }
        DeliverError::NoResumable => (
            StatusCode::CONFLICT,
            format!("worktree {id} has no resumable session"),
        )
            .into_response(),
        DeliverError::Spawn(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("worktree {id} resume failed: {e}"),
        )
            .into_response(),
        DeliverError::SendFailed => (
            StatusCode::BAD_GATEWAY,
            format!("worktree {id} input delivery failed"),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct CreateTaskBody {
    repo: String,
    prompt: String,
    branch: Option<String>,
    base: Option<String>,
    title: Option<String>,
    permission_mode: Option<String>,
    /// Orchestrator worktree id that's spawning this task, so the child links
    /// back into its fleet. Sent by the Flock MCP (from FLOCK_WORKTREE_ID).
    parent_id: Option<i64>,
    /// Claude `--model` override. Validated against `commands::ALLOWED_MODELS`.
    /// Required when `parent_id` is set (orchestrator-spawned); optional otherwise.
    model: Option<String>,
    /// Claude `--effort` override. Validated against `commands::ALLOWED_EFFORTS`.
    /// Required when `parent_id` is set (orchestrator-spawned); optional otherwise.
    effort: Option<String>,
    /// `"claude"` or `"codex"`. Required when `parent_id` is set
    /// (orchestrator-spawned); omitted otherwise → Claude. Codex only in Thanx repos.
    agent: Option<String>,
    /// Explicit override for the cross-account safety check: when `parent_id`
    /// is set and the target repo resolves to a different Claude account than
    /// the spawning orchestrator, `start_task_core` refuses the task unless
    /// this is `true`. Omit/false is the safe default.
    #[serde(default)]
    confirm_cross_account: bool,
}

#[derive(Serialize)]
struct CreatedTask {
    id: i64,
    branch: String,
    title: Option<String>,
    path: String,
}

/// Orchestration entry point: spawn a worktree + prompted claude session.
/// `{"repo":"<name>","prompt":"...","branch?","base?","title?","permission_mode?"}`.
/// This is what lets a loop (cron, script, or another agent) create work.
async fn create_task(State(ctx): State<ApiCtx>, Json(body): Json<CreateTaskBody>) -> Response {
    let st = ctx.app.state::<AppState>();
    let repo_id = st
        .db
        .list_repos()
        .ok()
        .and_then(|repos| repos.into_iter().find(|r| r.name == body.repo).map(|r| r.id));
    let Some(repo_id) = repo_id else {
        return (StatusCode::BAD_REQUEST, format!("unknown repo {:?}", body.repo)).into_response();
    };
    if let Err(e) = crate::commands::require_explicit_model_and_effort(
        body.parent_id,
        body.model.as_deref(),
        body.effort.as_deref(),
    )
    .and_then(|()| crate::commands::require_explicit_agent(body.parent_id, body.agent.as_deref()))
    {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    // Git + tmux work is blocking — keep it off the async executor.
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        crate::commands::start_task_core(
            &app,
            &st,
            repo_id,
            &body.prompt,
            body.branch,
            body.base,
            body.title,
            body.permission_mode,
            body.parent_id,
            body.model,
            body.effort,
            body.agent,
            body.confirm_cross_account,
        )
    })
    .await;

    match res {
        Ok(Ok(w)) => Json(CreatedTask {
            id: w.id,
            branch: w.branch,
            title: w.title,
            path: w.path,
        })
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "task join failed").into_response(),
    }
}

async fn schedules_list(State(ctx): State<ApiCtx>) -> Json<Vec<Schedule>> {
    let st = ctx.app.state::<AppState>();
    Json(st.db.list_schedules().unwrap_or_default())
}

#[derive(Deserialize)]
struct CreateScheduleBody {
    repo: String,
    prompt: String,
    spec: String,
    title: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    /// `"claude"` or `"codex"`. Required when `parent_id` is set
    /// (orchestrator-created); omitted otherwise → Claude. Codex only in Thanx repos.
    agent: Option<String>,
    /// Orchestrator worktree id that's creating this schedule, so the
    /// cross-account guard can check it and the resulting fired tasks link
    /// into its fleet. Sent by the Flock MCP (from FLOCK_WORKTREE_ID).
    parent_id: Option<i64>,
    /// See `CreateTaskBody::confirm_cross_account`.
    #[serde(default)]
    confirm_cross_account: bool,
}

async fn schedule_create_h(
    State(ctx): State<ApiCtx>,
    Json(body): Json<CreateScheduleBody>,
) -> Response {
    let st = ctx.app.state::<AppState>();
    let repo_id = st
        .db
        .list_repos()
        .ok()
        .and_then(|rs| rs.into_iter().find(|r| r.name == body.repo).map(|r| r.id));
    let Some(repo_id) = repo_id else {
        return (StatusCode::BAD_REQUEST, format!("unknown repo {:?}", body.repo)).into_response();
    };
    match crate::commands::schedule_create_core(
        &st.db,
        repo_id,
        &body.prompt,
        &body.spec,
        body.title.as_deref(),
        body.model.as_deref(),
        body.effort.as_deref(),
        body.agent.as_deref(),
        body.parent_id,
        body.confirm_cross_account,
    ) {
        Ok(s) => Json(s).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn schedule_delete_h(State(ctx): State<ApiCtx>, Path(id): Path<i64>) -> StatusCode {
    let st = ctx.app.state::<AppState>();
    let _ = st.db.delete_schedule(id);
    StatusCode::NO_CONTENT
}

async fn schedule_run_h(State(ctx): State<ApiCtx>, Path(id): Path<i64>) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let s = st.db.get_schedule(id)?;
        let title = s
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| Some(format!("scheduled: {}", s.spec)));
        let w = crate::commands::start_task_core(
            &app,
            &st,
            s.repo_id,
            &s.prompt,
            None,
            None,
            title,
            None,
            s.parent_id,
            s.model.clone(),
            s.effort.clone(),
            // The schedule's own agent, as in commands::schedule_run_now.
            Some(s.agent.clone()),
            // Already gated at schedule_create time.
            true,
        )?;
        if let Some(spec) = crate::schedule::parse_spec(&s.spec) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let _ = st
                .db
                .mark_schedule_run(id, now, crate::schedule::next_run(&spec, now));
        }
        Ok::<_, crate::error::AppError>(w)
    })
    .await;
    match res {
        Ok(Ok(w)) => Json(CreatedTask {
            id: w.id,
            branch: w.branch,
            title: w.title,
            path: w.path,
        })
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

#[derive(Deserialize)]
struct TranscriptQuery {
    /// Byte offset into the session file already seen; only newer bytes are
    /// parsed and returned (incremental polling). 0/omitted = initial load.
    since: Option<u64>,
}

#[derive(Serialize)]
struct TranscriptResp {
    messages: Vec<crate::transcript::Msg>,
    bytes: u64,
}

/// Reader feed: the worktree's agent conversation as clean messages, parsed
/// from the session JSONL — Claude's transcript, or Codex's rollout for a Codex
/// worktree (read-only — never touches the live terminal). Poll with
/// `?since=<bytes>` to fetch only what's new.
async fn transcript_h(
    State(ctx): State<ApiCtx>,
    Path(id): Path<i64>,
    Query(q): Query<TranscriptQuery>,
) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let w = st.db.get_worktree(id).ok()?;
        // Resolve the worktree's env profile so the Reader reads from the same
        // CLAUDE_CONFIG_DIR the session writes to (work vs Personal split). A
        // persisted `env_profile` (an orchestrator's chosen account) wins over
        // path-based resolution — see `resolve_vars_for_worktree`.
        let env_vars = match st.db.get_repo(w.repo_id) {
            Ok(repo) => crate::env_profiles::resolve_vars_for_worktree(
                &crate::env_profiles::load(),
                w.env_profile.as_deref(),
                &repo.path,
            ),
            Err(_) => Vec::new(),
        };
        let codex = w.agent == crate::db::AGENT_CODEX;
        let file = if codex {
            crate::transcript::codex_session_for(
                &w.path,
                crate::transcript::codex_home_from_env(&env_vars),
            )?
            .0
        } else {
            crate::transcript::session_file_for(
                &w.path,
                crate::transcript::config_dir_from_env(&env_vars),
            )?
        };
        let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        let since = q.since.unwrap_or(0);
        let text = if since > 0 && since <= size {
            read_from(&file, since)
        } else {
            std::fs::read(&file)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        let mut msgs = if codex {
            crate::transcript::parse_codex_messages(&text)
        } else {
            crate::transcript::parse_messages(&text)
        };
        // Initial load: cap to the most recent slice so the payload is bounded.
        if since == 0 && msgs.len() > 150 {
            msgs = msgs.split_off(msgs.len() - 150);
        }
        Some(TranscriptResp { messages: msgs, bytes: size })
    })
    .await
    .ok()
    .flatten();
    match res {
        Some(r) => Json(r).into_response(),
        None => Json(TranscriptResp {
            messages: vec![],
            bytes: 0,
        })
        .into_response(),
    }
}

fn read_from(path: &std::path::Path, offset: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

// ---------- knowledge base ----------

#[derive(Deserialize)]
struct KbSearchQ {
    q: String,
    limit: Option<i64>,
}

async fn kb_search_h(State(ctx): State<ApiCtx>, Query(q): Query<KbSearchQ>) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let query = crate::kb::sanitize_query(&q.q);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        st.db.kb_search(&query, q.limit.unwrap_or(20))
    })
    .await;
    match res {
        Ok(Ok(hits)) => Json(hits).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

#[derive(Deserialize)]
struct KbReadQ {
    path: String,
}

async fn kb_read_h(State(ctx): State<ApiCtx>, Query(q): Query<KbReadQ>) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        st.db.kb_get(&q.path)
    })
    .await;
    match res {
        Ok(Ok(doc)) => Json(doc).into_response(),
        Ok(Err(_)) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

#[derive(Deserialize)]
struct KbListQ {
    prefix: Option<String>,
    limit: Option<i64>,
}

async fn kb_list_h(State(ctx): State<ApiCtx>, Query(q): Query<KbListQ>) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        st.db.kb_list(q.prefix.as_deref(), q.limit.unwrap_or(100))
    })
    .await;
    match res {
        Ok(Ok(items)) => Json(items).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

#[derive(Deserialize)]
struct KbIngestBody {
    path: String,
    content: String,
}

async fn kb_ingest_h(State(ctx): State<ApiCtx>, Json(body): Json<KbIngestBody>) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        crate::kb::ingest_content(
            &st.db,
            crate::kb::vault_path().as_deref(),
            &body.path,
            &body.content,
        )
    })
    .await;
    match res {
        Ok(Ok(path)) => Json(serde_json::json!({ "ok": true, "path": path })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

#[derive(Deserialize)]
struct KbDeleteBody {
    path: String,
}

async fn kb_delete_h(State(ctx): State<ApiCtx>, Json(body): Json<KbDeleteBody>) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        crate::kb::delete_doc(&st.db, crate::kb::vault_path().as_deref(), &body.path)
    })
    .await;
    match res {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

async fn kb_reindex_h(State(ctx): State<ApiCtx>) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        match crate::kb::vault_path() {
            Some(v) => crate::kb::reindex(&st.db, &v),
            None => Ok(0),
        }
    })
    .await;
    match res {
        Ok(Ok(count)) => Json(serde_json::json!({ "indexed": count })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "join failed").into_response(),
    }
}

fn build_router(ctx: ApiCtx) -> Router {
    let api = Router::new()
        .route("/worktrees", get(worktrees))
        .route("/worktrees/:id/input", post(input))
        .route("/worktrees/:id/transcript", get(transcript_h))
        .route("/worktrees/:id/remove", post(remove_worktree_h))
        .route("/tasks", post(create_task))
        .route("/notify", post(notify_h))
        .route("/schedules", get(schedules_list).post(schedule_create_h))
        .route("/schedules/:id", delete(schedule_delete_h))
        .route("/schedules/:id/run", post(schedule_run_h))
        .route("/status", get(status_counts))
        .route("/kb/search", get(kb_search_h))
        .route("/kb/read", get(kb_read_h))
        .route("/kb/list", get(kb_list_h))
        .route("/kb/ingest", post(kb_ingest_h))
        .route("/kb/delete", post(kb_delete_h))
        .route("/kb/reindex", post(kb_reindex_h))
        .route_layer(middleware::from_fn_with_state(ctx.clone(), require_auth));
    Router::new()
        .nest("/api", api)
        .with_state(ctx)
}

fn spawn_serve(listener: TcpListener, router: Router, cancel: CancellationToken) {
    tauri::async_runtime::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service())
            .with_graceful_shutdown(async move {
                cancel.cancelled().await;
            })
            .await;
    });
}

// ---------- commands ----------

#[tauri::command]
pub fn remote_start(app: AppHandle) -> Result<RemoteInfo, String> {
    let token = load_or_create_token().map_err(|e| e.to_string())?;
    let st = app.state::<AppState>();
    let mut guard = st.remote.lock().unwrap();
    if guard.is_none() {
        // A bind failure (e.g. port busy) is surfaced.
        let listener = tauri::async_runtime::block_on(TcpListener::bind(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            PORT,
        ))))
        .map_err(|e| format!("bind 127.0.0.1:{PORT}: {e}"))?;
        let ctx = ApiCtx {
            app: app.clone(),
            token: Arc::new(token.clone()),
        };
        let router = build_router(ctx);
        let cancel = CancellationToken::new();
        spawn_serve(listener, router, cancel.clone());
        *guard = Some(cancel);
    }
    drop(guard);
    Ok(RemoteInfo {
        running: true,
        token,
    })
}

#[tauri::command]
pub fn remote_stop(app: AppHandle) -> RemoteInfo {
    if let Some(cancel) = app.state::<AppState>().remote.lock().unwrap().take() {
        cancel.cancel();
    }
    let token = load_or_create_token().unwrap_or_default();
    RemoteInfo {
        running: false,
        token,
    }
}

#[tauri::command]
pub fn remote_info(app: AppHandle) -> RemoteInfo {
    let running = app.state::<AppState>().remote.lock().unwrap().is_some();
    let token = load_or_create_token().unwrap_or_default();
    RemoteInfo {
        running,
        token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_matches_is_exact() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc123", "abc124"));
        assert!(!token_matches("abc", "abc123")); // length mismatch
        assert!(!token_matches("", "x"));
    }

    #[test]
    fn gen_token_is_urlsafe_and_long() {
        let t = gen_token();
        assert!(t.len() >= 40);
        assert!(t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn map_key_allowlist() {
        assert_eq!(map_key("enter"), Some("Enter"));
        assert_eq!(map_key("Esc"), Some("Escape"));
        assert_eq!(map_key("shift-tab"), Some("BTab"));
        assert_eq!(map_key("ctrl-c"), Some("C-c"));
        assert_eq!(map_key("rm -rf"), None);
        assert_eq!(map_key(""), None);
    }

    #[test]
    fn remove_refusals_map_to_clear_codes() {
        use crate::commands::RemoveRefusal;
        assert_eq!(remove_refusal_response(RemoveRefusal::NotFound, 1).status(), StatusCode::NOT_FOUND);
        assert_eq!(
            remove_refusal_response(RemoveRefusal::IsOrchestrator, 1).status(),
            StatusCode::FORBIDDEN
        );
        let d = crate::git::DirtySummary { staged: 1, unstaged: 0, untracked: 2 };
        assert_eq!(remove_refusal_response(RemoveRefusal::Dirty(d), 1).status(), StatusCode::CONFLICT);
        // The user's "no" and no answer at all are both refusals, never a 2xx.
        assert_eq!(remove_refusal_response(RemoveRefusal::Declined, 1).status(), StatusCode::FORBIDDEN);
        assert_eq!(
            remove_refusal_response(RemoveRefusal::TimedOut, 1).status(),
            StatusCode::REQUEST_TIMEOUT
        );
    }

    #[test]
    fn input_errors_map_to_clear_codes_never_502() {
        use crate::commands::DeliverError;
        // A dead / unknown / unresumable session must surface a clear status,
        // never the opaque 502 that made the orchestrator escalate to infra.
        assert_eq!(
            input_error_response(DeliverError::NotFound, 1).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            input_error_response(DeliverError::NoResumable, 2).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            input_error_response(DeliverError::Spawn("boom".into()), 3).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        for e in [
            DeliverError::NotFound,
            DeliverError::NoResumable,
            DeliverError::Spawn("x".into()),
        ] {
            assert_ne!(input_error_response(e, 9).status(), StatusCode::BAD_GATEWAY);
        }
        // SendFailed (live session, tmux refused the key) is the only residual
        // 502 — genuinely unexpected, not the dead-session path.
        assert_eq!(
            input_error_response(DeliverError::SendFailed, 4).status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn notify_errors_map_to_clear_codes() {
        use crate::notify::NotifyError;
        assert_eq!(notify_error_response(NotifyError::NotFound, 1).status(), StatusCode::NOT_FOUND);
        assert_eq!(notify_error_response(NotifyError::NoParent, 1).status(), StatusCode::CONFLICT);
        assert_eq!(
            notify_error_response(NotifyError::Invalid("x".into()), 1).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn extract_token_is_bearer_header_only() {
        let req = |uri: &str, auth: Option<&str>| {
            let mut b = Request::builder().uri(uri);
            if let Some(a) = auth {
                b = b.header(header::AUTHORIZATION, a);
            }
            b.body(axum::body::Body::empty()).unwrap()
        };
        assert_eq!(
            extract_token(&req("/api/status", Some("Bearer abc"))).as_deref(),
            Some("abc")
        );
        // The query-string token existed only for the PWA's EventSource.
        assert_eq!(extract_token(&req("/api/status?token=abc", None)), None);
    }
}
