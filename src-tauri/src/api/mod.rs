//! Remote API + PWA server (phase 3a).
//!
//! An axum server, opt-in via the desktop Settings toggle, that exposes the
//! worktree list + live status and serves the installable PWA shell. Security
//! posture (Thanx policy): binds `127.0.0.1` (always) + the Tailscale IP
//! (best-effort) only — never `0.0.0.0`, so untrusted LANs can't reach it even
//! with the token. All `/api/*` routes require the master token (Bearer header
//! or `?token=` for EventSource, which can't set headers).

use crate::monitor::WorktreeStatus;
use crate::state::AppState;
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use serde::Serialize;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
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

fn token_path() -> std::io::Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| std::io::Error::other("no data local dir"))?
        .join("Flock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("api-token"))
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

async fn index() -> impl IntoResponse {
    Html(INDEX_HTML)
}
async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], APP_JS)
}
async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST,
    )
}
async fn service_worker() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], SW_JS)
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

fn build_router(ctx: ApiCtx) -> Router {
    let api = Router::new()
        .route("/worktrees", get(worktrees))
        .route("/worktrees/:id/stream", get(stream))
        .route("/status", get(status_counts))
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
    fn query_param_extracts_token() {
        assert_eq!(query_param("token=abc", "token").as_deref(), Some("abc"));
        assert_eq!(
            query_param("a=1&token=xyz&b=2", "token").as_deref(),
            Some("xyz")
        );
        assert_eq!(query_param("a=1&b=2", "token"), None);
    }
}
