//! Review-only checkouts and immutable, expiring GitHub review previews.
use crate::{
    db::Worktree,
    env_profiles,
    error::{AppError, AppResult},
    review::{self, DiffFile, Target},
    state::AppState,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
}
pub fn parse_url(url: &str) -> AppResult<PrRef> {
    let clean = url
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    let parts: Vec<_> = clean
        .strip_prefix("https://github.com/")
        .unwrap_or("")
        .split('/')
        .collect();
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    if parts.len() != 4 || parts[2] != "pull" || !valid(parts[0]) || !valid(parts[1]) {
        return Err(AppError::msg(
            "Pegá una URL como https://github.com/organizacion/repo/pull/123",
        ));
    }
    let number = parts[3]
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| AppError::msg("El número del PR no es válido"))?;
    Ok(PrRef {
        owner: parts[0].into(),
        repo: parts[1].into(),
        number,
        url: format!("https://github.com/{}/{}/pull/{number}", parts[0], parts[1]),
    })
}
fn remote_slug(remote: &str) -> Option<String> {
    let s = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = if let Some((_, path)) = s.split_once("://") {
        path.split_once('/')?.1
    } else {
        s.split_once(':')?.1
    };
    let p: Vec<_> = path.split('/').collect();
    if p.len() != 2 {
        return None;
    }
    Some(format!("{}/{}", p[0], p[1]).to_lowercase())
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Source {
    pub pr: PrRef,
    pub checkout: bool,
    pub head_sha: String,
    pub base_sha: String,
    pub title: String,
    pub body: String,
}
fn source_path(id: i64) -> AppResult<PathBuf> {
    Ok(review::data_dir()?.join(id.to_string()).join("source.json"))
}
pub fn source(id: i64) -> AppResult<Source> {
    serde_json::from_slice(&std::fs::read(source_path(id)?)?)
        .map_err(|e| AppError::msg(e.to_string()))
}
pub fn source_head(w: &Worktree) -> AppResult<String> {
    Ok(source(w.id)?.head_sha)
}
pub fn files(w: &Worktree, base: &str, head: &str) -> AppResult<Vec<DiffFile>> {
    if source(w.id)?.checkout {
        review::diff_between(Path::new(&w.path), base, head)
    } else {
        if !review::valid_sha(head) {
            return Err(AppError::msg("Commit inválido"));
        }
        Ok(review::parse_diff(&std::fs::read_to_string(
            review::data_dir()?
                .join(w.id.to_string())
                .join(format!("{head}.diff")),
        )?))
    }
}
pub fn metadata(cwd: &Path, pr: &PrRef, env: &[(String, String)]) -> AppResult<Value> {
    let raw = review::run(
        cwd,
        "gh",
        &[
            "pr",
            "view",
            &pr.url,
            "--json",
            "title,body,url,headRefOid,baseRefOid",
        ],
        env,
    )?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| AppError::msg(e.to_string()))?;
    if !review::valid_sha(v["headRefOid"].as_str().unwrap_or(""))
        || !review::valid_sha(v["baseRefOid"].as_str().unwrap_or(""))
    {
        return Err(AppError::msg("GitHub no devolvió los commits del PR"));
    }
    Ok(v)
}
fn sources_root() -> AppResult<PathBuf> {
    let root = review::data_dir()?
        .parent()
        .unwrap()
        .join("review-worktrees");
    std::fs::create_dir_all(&root)?;
    Ok(root)
}
pub fn open_pr(app: &AppHandle, url: &str) -> AppResult<Worktree> {
    // Serialize opens so a desktop/MCP double click never creates two checkouts.
    static OPEN: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = OPEN.get_or_init(Default::default).lock().unwrap();
    let pr = parse_url(url)?;
    let state = app.state::<AppState>();
    let repos = state.db.list_repos()?;
    for repo in &repos {
        for w in state.db.list_worktrees(repo.id)? {
            if w.kind == "review" && source(w.id).is_ok_and(|s| s.pr == pr) {
                refresh(&state, &w)?;
                return Ok(w);
            }
        }
    }
    let slug = format!("{}/{}", pr.owner, pr.repo).to_lowercase();
    let registered = repos.iter().find(|r| {
        review::run(
            Path::new(&r.path),
            "git",
            &["remote", "get-url", "origin"],
            &[],
        )
        .ok()
        .and_then(|s| remote_slug(&s))
        .as_deref()
            == Some(&slug)
    });
    let root = sources_root()?;
    let repo = if let Some(r) = registered {
        r.clone()
    } else {
        state
            .db
            .insert_repo("PRs para revisar", &root.to_string_lossy())?
    };
    let env = env_profiles::resolve_vars(&env_profiles::load(), &repo.path);
    let cwd = Path::new(&repo.path);
    let info = metadata(cwd, &pr, &env)?;
    let sha = info["headRefOid"].as_str().unwrap();
    let path = root.join(format!(
        "{}-{}-{}-{}",
        pr.owner,
        pr.repo,
        pr.number,
        rand::random::<u32>()
    ));
    if registered.is_some() {
        review::run(
            cwd,
            "git",
            &["fetch", "origin", &format!("pull/{}/head", pr.number)],
            &env,
        )?;
        let fetched = review::run(cwd, "git", &["rev-parse", "FETCH_HEAD"], &env)?;
        if fetched.trim() != sha {
            return Err(AppError::msg(
                "El PR cambió mientras se abría. Volvé a abrirlo.",
            ));
        }
        review::run(
            cwd,
            "git",
            &["worktree", "add", "--detach", &path.to_string_lossy(), sha],
            &env,
        )?;
    } else {
        std::fs::create_dir_all(&path)?;
    }
    let inserted = state.db.insert_worktree(
        repo.id,
        &format!("PR #{}", pr.number),
        &path.to_string_lossy(),
        Some(&format!("{} · PR #{}", pr.repo, pr.number)),
        "default",
        "review",
        None,
        None,
        None,
        None,
        "claude",
    );
    let w = match inserted {
        Ok(w) => w,
        Err(e) => {
            if registered.is_some() {
                let _ = review::run(
                    cwd,
                    "git",
                    &["worktree", "remove", &path.to_string_lossy()],
                    &env,
                );
            } else {
                let _ = std::fs::remove_dir(&path);
            }
            return Err(e);
        }
    };
    let initial = Source {
        pr,
        checkout: registered.is_some(),
        head_sha: sha.into(),
        base_sha: info["baseRefOid"].as_str().unwrap().into(),
        title: info["title"].as_str().unwrap_or("").into(),
        body: info["body"].as_str().unwrap_or("").into(),
    };
    review::write_json(&source_path(w.id)?, &initial)?;
    if let Err(e) = refresh(&state, &w) {
        let _ = state.db.delete_worktree(w.id);
        if initial.checkout {
            let _ = review::run(
                cwd,
                "git",
                &["worktree", "remove", &path.to_string_lossy()],
                &env,
            );
        } else {
            let _ = std::fs::remove_dir(&path);
        }
        return Err(e);
    }
    let _ = app.emit("worktree:created", w.clone());
    Ok(w)
}
pub fn refresh(state: &AppState, w: &Worktree) -> AppResult<()> {
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<i64, std::sync::Arc<Mutex<()>>>>> =
        OnceLock::new();
    let lock = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(w.id)
        .or_default()
        .clone();
    let _guard = lock.lock().unwrap();
    let mut s = source(w.id)?;
    let env = review::worktree_env(state, w)?;
    let cwd = Path::new(&w.path);
    let info = metadata(cwd, &s.pr, &env)?;
    let sha = info["headRefOid"].as_str().unwrap();
    let base = info["baseRefOid"].as_str().unwrap();
    // Resolve the PR's merge base, including stacked PR bases, rather than the default tip.
    if s.checkout {
        review::run(
            cwd,
            "git",
            &["fetch", "origin", &format!("pull/{}/head", s.pr.number)],
            &env,
        )?;
        let fetched = review::run(cwd, "git", &["rev-parse", "FETCH_HEAD"], &env)?;
        if fetched.trim() != sha {
            return Err(AppError::msg(
                "El PR cambió durante la lectura. Volvé a actualizarlo.",
            ));
        }
        review::run(cwd, "git", &["fetch", "origin", base], &env)?;
        s.base_sha = review::run(cwd, "git", &["merge-base", base, sha], &env)?
            .trim()
            .into();
        review::run(cwd, "git", &["checkout", "--detach", sha], &env)?;
    } else {
        let diff = review::run(cwd, "gh", &["pr", "diff", &s.pr.url, "--color=never"], &env)?;
        let after = metadata(cwd, &s.pr, &env)?;
        if after["headRefOid"] != info["headRefOid"] || after["baseRefOid"] != info["baseRefOid"] {
            return Err(AppError::msg(
                "El PR cambió durante la lectura. Volvé a actualizarlo.",
            ));
        }
        std::fs::create_dir_all(review::data_dir()?.join(w.id.to_string()))?;
        std::fs::write(
            review::data_dir()?
                .join(w.id.to_string())
                .join(format!("{sha}.diff")),
            diff,
        )?;
        s.base_sha = base.into();
    }
    s.head_sha = sha.into();
    s.title = info["title"].as_str().unwrap_or("").into();
    s.body = info["body"].as_str().unwrap_or("").into();
    review::write_json(&source_path(w.id)?, &s)
}
pub fn target(w: &Worktree) -> AppResult<Target> {
    let s = source(w.id)?;
    Ok(Target {
        repo: s.pr.repo,
        own: false,
        worktree_id: w.id,
        pr_url: Some(s.pr.url),
        base_sha: s.base_sha,
        head_sha: s.head_sha,
    })
}

pub fn find(state: &AppState, url: &str) -> AppResult<Option<Worktree>> {
    let pr = parse_url(url)?;
    for repo in state.db.list_repos()? {
        for w in state.db.list_worktrees(repo.id)? {
            if w.kind == "review" && source(w.id).is_ok_and(|s| s.pr == pr) {
                return Ok(Some(w));
            }
            if w.kind == "worktree"
                && review::load(state, w.id)
                    .ok()
                    .and_then(|v| v.review)
                    .is_some_and(|r| r.target.pr_url.as_deref() == Some(&pr.url))
            {
                return Ok(Some(w));
            }
        }
    }
    Ok(None)
}
pub fn resolve_cached(
    state: &AppState,
    id: Option<i64>,
    url: Option<String>,
) -> AppResult<Worktree> {
    match (id, url) {
        (Some(id), None) => state.db.get_worktree(id),
        (None, Some(url)) => {
            find(state, &url)?.ok_or_else(|| AppError::msg("Abrí primero ese PR con review_open"))
        }
        _ => Err(AppError::msg("Indicá worktree_id o pr_url")),
    }
}
pub fn posting_target(
    state: &AppState,
    w: &Worktree,
) -> AppResult<(Target, PrRef, Vec<DiffFile>, String)> {
    let cwd = Path::new(&w.path);
    let env = review::worktree_env(state, w)?;
    let (target, files) = if w.kind == "review" {
        refresh(state, w)?;
        let t = target(w)?;
        let f = files(w, &t.base_sha, &t.head_sha)?;
        (t, f)
    } else {
        let v = review::load_selected(state, w.id, None)?;
        let r = v
            .review
            .ok_or_else(|| AppError::msg("Abrí primero la explicación de este PR"))?;
        if r.target.head_sha != v.head_sha {
            return Err(AppError::msg(
                "Hay commits nuevos. Pedí que actualicen la explicación antes del preview.",
            ));
        }
        (r.target, v.files)
    };
    let url = target
        .pr_url
        .as_deref()
        .ok_or_else(|| AppError::msg("Este worktree todavía no tiene un PR"))?;
    let pr = parse_url(url)?;
    let info = metadata(cwd, &pr, &env)?;
    if info["headRefOid"].as_str() != Some(&target.head_sha) {
        return Err(AppError::msg(
            "Los commits de GitHub cambiaron. Pedí que actualicen la explicación.",
        ));
    }
    Ok((
        target,
        pr,
        files,
        info["baseRefOid"].as_str().unwrap().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn github_urls_are_canonical_and_cannot_be_command_arguments() {
        let pr = parse_url("https://github.com/Thanx/ordering/pull/42/files?diff=split#x");
        assert!(pr.is_err());
        let pr = parse_url("https://github.com/Thanx/ordering/pull/42/?x=1#y").unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.url, "https://github.com/Thanx/ordering/pull/42");
        for bad in [
            "https://example.com/o/r/pull/1",
            "https://github.com/o/r/pull/0",
            "https://github.com/o/../pull/1",
            "https://github.com/o/r/pull/1;touch",
            "--help",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn registered_repos_match_ssh_aliases_and_https_remotes() {
        assert_eq!(
            remote_slug("git@github.com-work:Thanx/ordering.git"),
            Some("thanx/ordering".into())
        );
        assert_eq!(
            remote_slug("https://github.com/thanx/ordering.git"),
            Some("thanx/ordering".into())
        );
        assert_eq!(remote_slug("/tmp/local/repo"), None);
    }
}
