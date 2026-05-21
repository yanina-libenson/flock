use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_git<I, S>(cwd: &Path, args: I) -> AppResult<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    // Prevent git from hanging on a credential or passphrase prompt when the
    // GUI can't forward stdin. If auth is needed, git will fail fast instead.
    let out = Command::new("git")
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(AppError::Git(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Verify a path is a git repo; return the canonical toplevel.
pub fn canonical_repo_root(path: &Path) -> AppResult<PathBuf> {
    let out = run_git(path, ["rev-parse", "--show-toplevel"])?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(AppError::Git("not a git repository".into()));
    }
    Ok(PathBuf::from(trimmed))
}

pub fn current_branch(path: &Path) -> AppResult<String> {
    let out = run_git(path, ["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok(out.trim().to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub bare: bool,
    pub detached: bool,
}

/// Parse `git worktree list --porcelain`.
pub fn list_worktrees(repo: &Path) -> AppResult<Vec<WorktreeEntry>> {
    let out = run_git(repo, ["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(e) = cur.take() {
                entries.push(e);
            }
            cur = Some(WorktreeEntry {
                path: rest.to_string(),
                branch: None,
                head: None,
                bare: false,
                detached: false,
            });
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            if let Some(e) = cur.as_mut() {
                e.head = Some(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("branch ") {
            if let Some(e) = cur.as_mut() {
                let short = rest.strip_prefix("refs/heads/").unwrap_or(rest);
                e.branch = Some(short.to_string());
            }
        } else if line == "bare" {
            if let Some(e) = cur.as_mut() {
                e.bare = true;
            }
        } else if line == "detached" {
            if let Some(e) = cur.as_mut() {
                e.detached = true;
            }
        }
    }
    if let Some(e) = cur.take() {
        entries.push(e);
    }
    Ok(entries)
}

/// Add a new worktree at `path` checking out new branch `branch` from `base`.
pub fn add_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    base: Option<&str>,
    new_branch: bool,
) -> AppResult<()> {
    let path_str = path.to_string_lossy().to_string();
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if new_branch {
        args.push("-b".into());
        args.push(branch.into());
        args.push(path_str);
        if let Some(b) = base {
            args.push(b.into());
        }
    } else {
        // check out existing branch
        args.push(path_str);
        args.push(branch.into());
    }
    run_git(repo, args)?;
    Ok(())
}

pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> AppResult<()> {
    let mut args: Vec<String> = vec!["worktree".into(), "remove".into()];
    if force {
        args.push("--force".into());
    }
    args.push(path.to_string_lossy().to_string());
    run_git(repo, args)?;
    Ok(())
}

/// Detect the repo's default branch (main / master / something else).
/// Checks origin/HEAD first, then falls back to common names.
pub fn detect_default_branch(repo: &Path) -> AppResult<String> {
    if let Ok(out) = run_git(repo, ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        let trimmed = out.trim();
        if let Some(b) = trimmed.strip_prefix("origin/") {
            if !b.is_empty() {
                return Ok(b.to_string());
            }
        }
    }
    for candidate in ["main", "master", "trunk", "develop"] {
        if run_git(
            repo,
            [
                "rev-parse",
                "--verify",
                &format!("refs/heads/{candidate}"),
            ],
        )
        .is_ok()
        {
            return Ok(candidate.to_string());
        }
    }
    Err(AppError::Git("could not detect default branch".into()))
}

pub fn fetch_branch(repo: &Path, remote: &str, branch: &str) -> AppResult<()> {
    run_git(repo, ["fetch", "--no-tags", remote, branch])?;
    Ok(())
}

pub fn list_branches(repo: &Path) -> AppResult<Vec<String>> {
    let out = run_git(
        repo,
        ["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )?;
    Ok(out
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Local + remote branches, deduplicated. Used for the "check out existing
/// branch" picker. Local names are kept verbatim; remote-only names have
/// their remote prefix (e.g. `origin/`) stripped.
pub fn list_all_branches(repo: &Path) -> AppResult<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    let mut branches = Vec::new();

    // Locals first (takes precedence on dedup).
    let locals = run_git(
        repo,
        ["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )?;
    for line in locals.lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        if seen.insert(s.to_string()) {
            branches.push(s.to_string());
        }
    }

    // Remotes: strip the remote name (first path segment) so "origin/foo" → "foo".
    let remotes = run_git(
        repo,
        ["for-each-ref", "--format=%(refname:short)", "refs/remotes/"],
    )?;
    for line in remotes.lines() {
        let s = line.trim();
        if s.is_empty() || s.ends_with("/HEAD") {
            continue;
        }
        if let Some((_, branch)) = s.split_once('/') {
            if seen.insert(branch.to_string()) {
                branches.push(branch.to_string());
            }
        }
    }
    Ok(branches)
}

/// Return (staged+unstaged+untracked counts) — quick dirty check.
pub fn dirty_summary(path: &Path) -> AppResult<DirtySummary> {
    let out = run_git(path, ["status", "--porcelain"])?;
    let mut staged = 0;
    let mut unstaged = 0;
    let mut untracked = 0;
    for line in out.lines() {
        if line.starts_with("??") {
            untracked += 1;
        } else {
            let mut chars = line.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            if x != ' ' && x != '?' {
                staged += 1;
            }
            if y != ' ' && y != '?' {
                unstaged += 1;
            }
        }
    }
    Ok(DirtySummary {
        staged,
        unstaged,
        untracked,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtySummary {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
}

/// Default worktree location for a repo: adjacent folder `.flock-worktrees/<repo>-<slug>`.
pub fn default_worktree_path(repo: &Path, slug: &str) -> AppResult<PathBuf> {
    let parent = repo
        .parent()
        .ok_or_else(|| AppError::msg("repo has no parent dir"))?;
    let name = repo
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo");
    Ok(parent.join(".flock-worktrees").join(format!("{name}-{slug}")))
}

pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out = "branch".to_string();
    }
    out
}
