//! Remote API + PWA server (phase 3a).
//!
//! An axum server, opt-in via the desktop Settings toggle, that exposes the
//! worktree list + live status and serves the installable PWA shell. Security
//! posture: binds `127.0.0.1` (always) + the Tailscale IP
//! (best-effort) only — never `0.0.0.0`, so untrusted LANs can't reach it even
//! with the token. All `/api/*` routes require the master token (Bearer header
//! or `?token=` for EventSource, which can't set headers).

use crate::db::Schedule;
use crate::monitor::WorktreeStatus;
use crate::state::AppState;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use p256::SecretKey;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use web_push::{
    ContentEncoding, IsahcWebPushClient, SubscriptionInfo, VapidSignatureBuilder, WebPushClient,
    WebPushMessageBuilder,
};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tauri::{AppHandle, Manager};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Flock's API port. Distinct from argus's 7743 so both can run side by side.
const PORT: u16 = 7765;

// PWA shell, embedded so it works identically in dev and a bundled .app.
const INDEX_HTML: &str = include_str!("../../../pwa/index.html");
const APP_JS: &str = include_str!("../../../pwa/app.js");
const MANIFEST: &str = include_str!("../../../pwa/manifest.webmanifest");
const SW_JS: &str = include_str!("../../../pwa/sw.js");
const XTERM_JS: &str = include_str!("../../../pwa/vendor/xterm.js");
const XTERM_CSS: &str = include_str!("../../../pwa/vendor/xterm.css");
const ADDON_FIT_JS: &str = include_str!("../../../pwa/vendor/addon-fit.js");

/// Shell asset body. In debug, read live from disk so PWA edits don't need a
/// rebuild (the assets are otherwise compiled into the binary via include_str!,
/// and `tauri dev` doesn't watch the pwa/ folder). In release, use the embedded
/// copy.
#[cfg(debug_assertions)]
fn shell_asset(rel: &str, embedded: &'static str) -> String {
    let path = format!("{}/../pwa/{}", env!("CARGO_MANIFEST_DIR"), rel);
    std::fs::read_to_string(path).unwrap_or_else(|_| embedded.to_string())
}
#[cfg(not(debug_assertions))]
fn shell_asset(_rel: &str, embedded: &'static str) -> String {
    embedded.to_string()
}

#[derive(Clone)]
struct ApiCtx {
    app: AppHandle,
    token: Arc<String>,
}

#[derive(Serialize)]
pub struct RemoteInfo {
    pub running: bool,
    pub token: String,
    pub urls: Vec<String>,
}

#[derive(Serialize)]
struct WorktreeRow {
    id: i64,
    repo: String,
    branch: String,
    title: Option<String>,
    status: Option<WorktreeStatus>,
    has_session: bool,
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

// ---------- tailscale ----------

/// The host's Tailscale IPv4, or None when Tailscale isn't installed/up. Via
/// the login shell so the GUI app's minimal PATH still finds the CLI.
fn tailscale_ipv4() -> Option<Ipv4Addr> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let out = std::process::Command::new(shell)
        .args(["-i", "-l", "-c", "tailscale ip -4 2>/dev/null"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn build_urls(token: &str) -> Vec<String> {
    let mut urls = vec![format!("http://127.0.0.1:{PORT}/?token={token}")];
    if let Some(ip) = tailscale_ipv4() {
        urls.push(format!("http://{ip}:{PORT}/?token={token}"));
    }
    urls
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
    req.uri().query().and_then(|q| query_param(q, "token"))
}

/// Minimal `key=value&…` lookup. The token is base64url-no-pad (`A–Za–z0–9-_`),
/// so no percent-decoding is needed.
fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().map(|v| v.to_string());
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
                    });
                }
            }
        }
    }
    Json(out)
}

async fn repos(State(ctx): State<ApiCtx>) -> Json<Vec<String>> {
    let st = ctx.app.state::<AppState>();
    let names = st
        .db
        .list_repos()
        .map(|rs| rs.into_iter().map(|r| r.name).collect())
        .unwrap_or_default();
    Json(names)
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

/// SSE live terminal: emits a base64 `capture-pane -e` snapshot of the
/// session whenever the rendered screen changes (polled ~2.5 fps server-side).
/// Snapshots are full screens — the PWA clears and repaints each frame. This
/// sidesteps tmux's attach-client sizing (a phone viewer can't shrink the
/// desktop) and supports multiple viewers. An `exit` event ends the stream
/// when the session is gone.
async fn stream(Path(id): Path<i64>) -> impl IntoResponse {
    let body = async_stream::stream! {
        let mut last = String::new();
        let mut first = true;
        let mut ticker = tokio::time::interval(Duration::from_millis(400));
        loop {
            ticker.tick().await;
            let cap = tokio::task::spawn_blocking(move || crate::pty::tmux_capture_pane_ansi(id))
                .await
                .ok()
                .flatten();
            match cap {
                Some(screen) => {
                    if first || screen != last {
                        first = false;
                        last = screen.clone();
                        let b64 = base64::engine::general_purpose::STANDARD.encode(screen.as_bytes());
                        yield Ok::<Event, Infallible>(Event::default().data(b64));
                    }
                }
                None => {
                    yield Ok::<Event, Infallible>(Event::default().event("exit").data(""));
                    break;
                }
            }
        }
    };
    Sse::new(body).keep_alive(KeepAlive::default())
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

/// How long to wait for a resumed session's Claude TUI to be ready before
/// delivering input. Generous on purpose — a cold `claude --resume` reloads
/// global config plus the transcript. On timeout we send anyway (the session
/// is live), so this only ever delays a send, never fails it.
const RESUME_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Why resume-on-input couldn't deliver. Every variant maps to a *clear* status
/// with a message — never the opaque 502 that a dead session used to produce.
enum InputError {
    /// No worktree row for this id.
    NotFound,
    /// Worktree exists but has no persisted Claude session to resume.
    NoResumable,
    /// Resuming the session (tmux spawn) failed.
    Spawn(String),
    /// Session is live but tmux refused the keystroke — genuinely unexpected.
    SendFailed,
}

/// Send input to a session: `{"text": "..."}` types literally, `{"key":"esc"}`
/// sends a special key. The agent's reply shows up on the SSE stream.
///
/// Resilient to a dead session: if the worktree's tmux session is gone
/// (hibernated by the monitor, reaped under memory pressure, lost to a reboot),
/// it is resumed transparently from Claude Code's on-disk transcript
/// (`claude --resume`) and the input is delivered once the session is ready. So
/// sending input "just works" whether the session was alive, idle, or dead. A
/// per-worktree lock makes two near-simultaneous inputs resume it exactly once.
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
    // action. The Enter goes as a separate send-keys after a short pause so the
    // TUI has finished ingesting the typed text before it's submitted (without
    // the gap, a fast Enter can be swallowed or land as a newline).
    let submit = literal && body.submit.unwrap_or(false);

    // Serialize input per worktree so two near-simultaneous calls to a dead
    // session resume it once (the second waits on the lock, then sees it live
    // and takes the fast path).
    let lock = {
        let st = ctx.app.state::<AppState>();
        let mut locks = st.input_locks.lock().unwrap();
        locks
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        // Resume the persisted session if tmux has none live for this worktree.
        // The live path is left untouched — no resume, no readiness wait.
        if !crate::pty::tmux_list_sessions().contains(&id) {
            let w = st.db.get_worktree(id).map_err(|_| InputError::NotFound)?;
            let cwd = std::path::Path::new(&w.path);
            let resume_id =
                crate::pty::latest_session_id(cwd).ok_or(InputError::NoResumable)?;
            // Resolve env-profile vars by the repo's path, mirroring
            // `session_open`'s desktop reattach.
            let env_vars = match st.db.get_repo(w.repo_id) {
                Ok(repo) => crate::env_profiles::resolve_vars(
                    &crate::env_profiles::load(),
                    &repo.path,
                ),
                Err(_) => Vec::new(),
            };
            // Headless resume — no PTY client (the REST caller has no viewer),
            // no `--append-system-prompt` (mirrors the desktop reattach; the
            // conversation comes back via `--resume`).
            crate::pty::start_detached(
                id,
                cwd,
                &w.permission_mode,
                &env_vars,
                None,
                None,
                Some(&resume_id),
            )
            .map_err(|e| InputError::Spawn(e.to_string()))?;
            // Wait for Claude's input UI before typing; on timeout, send anyway.
            crate::pty::wait_until_ready(id, RESUME_READY_TIMEOUT);
        }

        let sent = crate::pty::tmux_send(id, literal, &payload);
        let sent = if sent && submit {
            std::thread::sleep(Duration::from_millis(120));
            crate::pty::tmux_send(id, false, "Enter")
        } else {
            sent
        };
        if sent {
            Ok(())
        } else {
            Err(InputError::SendFailed)
        }
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

/// Map an `InputError` to a clear HTTP response. A dead, unknown, or
/// unresumable session yields 404 / 409 / 500 with a message — never an opaque
/// 502 (the pre-fix failure mode that made callers think the API was down).
fn input_error_response(err: InputError, id: i64) -> Response {
    match err {
        InputError::NotFound => {
            (StatusCode::NOT_FOUND, format!("worktree {id} not found")).into_response()
        }
        InputError::NoResumable => (
            StatusCode::CONFLICT,
            format!("worktree {id} has no resumable session"),
        )
            .into_response(),
        InputError::Spawn(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("worktree {id} resume failed: {e}"),
        )
            .into_response(),
        InputError::SendFailed => (
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
        let w =
            crate::commands::start_task_core(&app, &st, s.repo_id, &s.prompt, None, None, title, None, None)?;
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
struct ResizeBody {
    cols: u16,
    rows: u16,
}

/// Reflow the session to the phone's viewport so the agent re-renders at a
/// readable width. Does not touch the desktop pane — the desktop re-claims its
/// own width when its pane is next active.
async fn resize_window(Path(id): Path<i64>, Json(body): Json<ResizeBody>) -> StatusCode {
    let ok = tokio::task::spawn_blocking(move || crate::pty::tmux_resize_window(id, body.cols, body.rows))
        .await
        .unwrap_or(false);
    if ok {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::BAD_GATEWAY
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

/// Reader feed: the worktree's Claude conversation as clean messages, parsed
/// from the session JSONL (read-only — never touches the live terminal). Poll
/// with `?since=<bytes>` to fetch only what's new.
async fn transcript_h(
    State(ctx): State<ApiCtx>,
    Path(id): Path<i64>,
    Query(q): Query<TranscriptQuery>,
) -> Response {
    let app = ctx.app.clone();
    let res = tokio::task::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let w = st.db.get_worktree(id).ok()?;
        let file = crate::transcript::session_file_for(&w.path)?;
        let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        let since = q.since.unwrap_or(0);
        let text = if since > 0 && since <= size {
            read_from(&file, since)
        } else {
            std::fs::read(&file)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        let mut msgs = crate::transcript::parse_messages(&text);
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

async fn index() -> impl IntoResponse {
    Html(shell_asset("index.html", INDEX_HTML))
}
async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        shell_asset("app.js", APP_JS),
    )
}
async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        shell_asset("manifest.webmanifest", MANIFEST),
    )
}
async fn service_worker() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        shell_asset("sw.js", SW_JS),
    )
}
async fn xterm_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], XTERM_JS)
}
async fn xterm_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], XTERM_CSS)
}
async fn addon_fit_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], ADDON_FIT_JS)
}

// ---------- web push (VAPID) ----------

#[derive(Serialize, Deserialize, Clone)]
struct PushSub {
    endpoint: String,
    keys: PushKeys,
}

#[derive(Serialize, Deserialize, Clone)]
struct PushKeys {
    p256dh: String,
    auth: String,
}

fn vapid_path() -> std::io::Result<PathBuf> {
    Ok(flock_dir()?.join("vapid.pem"))
}

fn subs_path() -> std::io::Result<PathBuf> {
    Ok(flock_dir()?.join("push-subs.json"))
}

/// Read the VAPID private key PEM, generating + persisting (0600) a fresh
/// P-256 keypair on first use.
fn load_or_create_vapid() -> std::io::Result<String> {
    let path = vapid_path()?;
    if let Ok(pem) = std::fs::read_to_string(&path) {
        if !pem.trim().is_empty() {
            return Ok(pem);
        }
    }
    let mut rng = rand::rngs::OsRng;
    let sk = SecretKey::random(&mut rng);
    let pem = sk
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(std::io::Error::other)?
        .to_string();
    std::fs::write(&path, &pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(pem)
}

/// The VAPID public key as a base64url-no-pad uncompressed point — the
/// `applicationServerKey` the browser's `pushManager.subscribe` needs.
fn vapid_public_key_b64() -> Option<String> {
    let pem = load_or_create_vapid().ok()?;
    let sk = SecretKey::from_pkcs8_pem(&pem).ok()?;
    let point = sk.public_key().to_encoded_point(false);
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(point.as_bytes()))
}

fn load_subs() -> Vec<PushSub> {
    subs_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_subs(subs: &[PushSub]) -> std::io::Result<()> {
    let path = subs_path()?;
    std::fs::write(path, serde_json::to_string(subs).unwrap_or_else(|_| "[]".into()))
}

/// Fan a "needs input" notification out to every subscribed device. Best
/// effort: missing key / no subscriptions / send errors are swallowed. Called
/// by the monitor on the busy→needs_input transition (cooldown-gated there).
pub fn notify_needs_input(title: String, body: String) {
    let subs = load_subs();
    if subs.is_empty() {
        return;
    }
    let Ok(pem) = load_or_create_vapid() else {
        return;
    };
    let payload = serde_json::json!({ "title": title, "body": body }).to_string();
    tauri::async_runtime::spawn(async move {
        let Ok(client) = IsahcWebPushClient::new() else {
            return;
        };
        for sub in subs {
            let info = SubscriptionInfo::new(sub.endpoint, sub.keys.p256dh, sub.keys.auth);
            let sig = match VapidSignatureBuilder::from_pem(pem.as_bytes(), &info) {
                Ok(mut b) => {
                    b.add_claim("sub", "mailto:flock@localhost");
                    match b.build() {
                        Ok(s) => s,
                        Err(_) => continue,
                    }
                }
                Err(_) => continue,
            };
            let mut mb = WebPushMessageBuilder::new(&info);
            mb.set_payload(ContentEncoding::Aes128Gcm, payload.as_bytes());
            mb.set_vapid_signature(sig);
            if let Ok(msg) = mb.build() {
                let _ = client.send(msg).await;
            }
        }
    });
}

async fn vapid_public_key() -> impl IntoResponse {
    match vapid_public_key_b64() {
        Some(k) => (StatusCode::OK, k).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "no vapid key").into_response(),
    }
}

async fn push_subscribe(Json(sub): Json<PushSub>) -> StatusCode {
    let mut subs = load_subs();
    if !subs.iter().any(|s| s.endpoint == sub.endpoint) {
        subs.push(sub);
        let _ = save_subs(&subs);
    }
    StatusCode::NO_CONTENT
}

fn build_router(ctx: ApiCtx) -> Router {
    let api = Router::new()
        .route("/worktrees", get(worktrees))
        .route("/worktrees/:id/stream", get(stream))
        .route("/worktrees/:id/input", post(input))
        .route("/worktrees/:id/resize", post(resize_window))
        .route("/worktrees/:id/transcript", get(transcript_h))
        .route("/tasks", post(create_task))
        .route("/repos", get(repos))
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
        .route("/push/vapid-public-key", get(vapid_public_key))
        .route("/push/subscribe", post(push_subscribe))
        .route_layer(middleware::from_fn_with_state(ctx.clone(), require_auth));
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/vendor/xterm.js", get(xterm_js))
        .route("/vendor/xterm.css", get(xterm_css))
        .route("/vendor/addon-fit.js", get(addon_fit_js))
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
        // localhost is mandatory; a failure here (e.g. port busy) is surfaced.
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
        spawn_serve(listener, router.clone(), cancel.clone());
        // Tailscale is best-effort — a bind failure must not take the API down.
        if let Some(ip) = tailscale_ipv4() {
            if let Ok(l) =
                tauri::async_runtime::block_on(TcpListener::bind(SocketAddr::from((ip, PORT))))
            {
                spawn_serve(l, router, cancel.clone());
            }
        }
        *guard = Some(cancel);
    }
    drop(guard);
    Ok(RemoteInfo {
        running: true,
        urls: build_urls(&token),
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
        urls: build_urls(&token),
        token,
    }
}

#[tauri::command]
pub fn remote_info(app: AppHandle) -> RemoteInfo {
    let running = app.state::<AppState>().remote.lock().unwrap().is_some();
    let token = load_or_create_token().unwrap_or_default();
    RemoteInfo {
        running,
        urls: build_urls(&token),
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
    fn input_errors_map_to_clear_codes_never_502() {
        // A dead / unknown / unresumable session must surface a clear status,
        // never the opaque 502 that made the orchestrator escalate to infra.
        assert_eq!(
            input_error_response(InputError::NotFound, 1).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            input_error_response(InputError::NoResumable, 2).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            input_error_response(InputError::Spawn("boom".into()), 3).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        for e in [
            InputError::NotFound,
            InputError::NoResumable,
            InputError::Spawn("x".into()),
        ] {
            assert_ne!(input_error_response(e, 9).status(), StatusCode::BAD_GATEWAY);
        }
        // SendFailed (live session, tmux refused the key) is the only residual
        // 502 — genuinely unexpected, not the dead-session path.
        assert_eq!(
            input_error_response(InputError::SendFailed, 4).status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn query_param_extracts_token() {
        assert_eq!(query_param("token=abc", "token").as_deref(), Some("abc"));
        assert_eq!(
            query_param("a=1&token=xyz&b=2", "token").as_deref(),
            Some("xyz")
        );
        assert_eq!(query_param("a=1&b=2", "token"), None);
    }
}
