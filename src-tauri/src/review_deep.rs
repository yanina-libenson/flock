//! Deep reviews use a separate interactive tmux session: flock-<negative worktree id>.
use crate::{
    db::Worktree,
    error::{AppError, AppResult},
    pty, review, review_github,
    state::AppState,
};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};
use tauri::{AppHandle, Emitter};
#[derive(Serialize, Deserialize)]
struct Session {
    head_sha: String,
    prompt: String,
}
#[derive(Clone, Serialize)]
pub struct Started {
    pub worktree_id: i64,
    pub session: Worktree,
}
fn session_worktree(w: &Worktree) -> Worktree {
    let mut session = w.clone();
    session.id = -w.id;
    session.agent = "claude".into();
    session.model = Some("opus".into());
    session.effort = None;
    session.title = Some("Revisión profunda".into());
    session
}
pub fn start(app: &AppHandle, state: &AppState, id: i64) -> AppResult<Started> {
    static START: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = START.get_or_init(Default::default).lock().unwrap();
    let w = state.db.get_worktree(id)?;
    if id <= 0 || w.kind == "orchestrator" {
        return Err(AppError::msg("Elegí un worktree para revisar"));
    }
    if w.kind == "review" {
        review_github::refresh(state, &w)?;
    }
    let view = review::load(state, id)?;
    let previous: Option<Session> = std::fs::read(
        review::data_dir()?
            .join(id.to_string())
            .join("deep-session.json"),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if pty::tmux_list_sessions().contains(&-id)
        && previous
            .as_ref()
            .is_some_and(|p| p.head_sha == view.head_sha)
    {
        let started = Started {
            worktree_id: id,
            session: session_worktree(&w),
        };
        let _ = app.emit("review:deep_open", started.clone());
        return Ok(started);
    }
    if pty::tmux_list_sessions().contains(&-id) {
        state.pty.kill(-id).ok();
        pty::tmux_kill_session(-id);
    }
    let target = if w.kind == "review" {
        review_github::target(&w)?
    } else {
        let env = review::worktree_env(state, &w)?;
        let raw = review::run(
            Path::new(&w.path),
            "gh",
            &["pr", "view", "--json", "url"],
            &env,
        )?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| AppError::msg(e.to_string()))?;
        review::Target {
            repo: state.db.get_repo(w.repo_id)?.name,
            own: true,
            worktree_id: id,
            pr_url: v["url"].as_str().map(String::from),
            base_sha: review::merge_base(Path::new(&w.path), &env)?,
            head_sha: view.head_sha,
        }
    };
    let url = target
        .pr_url
        .as_deref()
        .ok_or_else(|| AppError::msg("Abrí el PR en GitHub antes de pedir la revisión profunda"))?;
    review_github::parse_url(url)?;
    let path = review::cache_path(&review::data_dir()?, id, &target.head_sha, true)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    review::write_json(&path.with_extension("target.json"), &target)?;
    let prompt = format!(
        "/review-pr {url} --flock-json {}",
        pty::shell_escape(&path.to_string_lossy())
    );
    let env = review::worktree_env(state, &w)?;
    review::write_json(
        &review::data_dir()?
            .join(id.to_string())
            .join("deep-session.json"),
        &Session {
            head_sha: target.head_sha,
            prompt: prompt.clone(),
        },
    )?;
    // Fresh first prompt; never resumes the author's long context.
    pty::start_detached(
        -id,
        Path::new(&w.path),
        "default",
        &env,
        Some(&prompt),
        None,
        None,
        Some("opus"),
        None,
        "claude",
    )?;
    let started = Started {
        worktree_id: id,
        session: session_worktree(&w),
    };
    let _ = app.emit("review:deep_open", started.clone());
    Ok(started)
}
pub fn attach(app: &AppHandle, state: &AppState, id: i64, cols: u16, rows: u16) -> AppResult<()> {
    let w = state.db.get_worktree(id)?;
    if !pty::tmux_list_sessions().contains(&-id) {
        return Err(AppError::msg(
            "La sesión profunda terminó. Pedí otra revisión profunda para empezar de nuevo.",
        ));
    }
    let env = review::worktree_env(state, &w)?;
    // Negative ids attach existing sessions only; pty never starts or resumes a model here.
    state.pty.attach(
        app,
        -id,
        Path::new(&w.path),
        cols,
        rows,
        "default",
        &env,
        None,
        None,
        Some("opus"),
        None,
        "claude",
    )
}
