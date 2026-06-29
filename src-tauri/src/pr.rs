//! Per-worktree PR lifecycle status.
//!
//! Derives "where is my task in the review loop" from `gh`, so the sidebar can
//! tell you at a glance whether the ball is on your end or not. A background
//! thread polls every worktree (whether or not it has a live session) and emits
//! `worktree:pr_status` whenever a worktree's state changes; a command exposes
//! the same computation on demand for the initial paint.
//!
//! Everything degrades silently: if `gh` is missing, unauthenticated, or the
//! repo isn't on GitHub, `compute` returns None and no badge shows.

use crate::git;
use crate::state::AppState;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// How often we re-check every worktree's PR state. Slow on purpose — these are
/// authenticated network calls, and review state changes on a human timescale.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Where a worktree's task sits in the PR lifecycle. Ordered loosely from
/// "nothing yet" through the review loop to "done". The frontend maps each to a
/// pill + color signalling whose turn it is.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Local commits exist but no PR yet — push & open one.
    ReadyToSubmit,
    /// PR open as a draft.
    Draft,
    /// PR open, CI still running.
    MonitoringCi,
    /// PR open, a required check failed.
    CiFailed,
    /// PR open, review requested, nothing blocking on your end.
    WaitingReview,
    /// PR open, unresolved review comment threads (human or bot).
    CommentsToAddress,
    /// PR open, a reviewer requested changes.
    ChangesRequested,
    /// PR open but conflicts with the base branch.
    Conflicts,
    /// Approved, mergeable, checks green — go merge it.
    ReadyToMerge,
    Merged,
    Closed,
}

#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct PrStatus {
    pub state: TaskState,
    pub number: Option<i64>,
    pub url: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct PrStatusEvent {
    pub worktree_id: i64,
    /// None clears the badge (no PR and nothing to submit).
    pub status: Option<PrStatus>,
}

/// Spawn the background poller. Runs for the life of the app on its own thread;
/// every poll is a cheap no-op when `gh` is unavailable.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        // Last emitted status per worktree — so we only emit on change.
        let mut last: HashMap<i64, Option<PrStatus>> = HashMap::new();
        let mut first = true;
        loop {
            // First pass runs immediately so the UI fills in soon after launch.
            if !first {
                std::thread::sleep(POLL_INTERVAL);
            }
            first = false;

            let Some(state) = app.try_state::<AppState>() else {
                continue;
            };
            let Ok(repos) = state.db.list_repos() else {
                continue;
            };

            let mut live: HashSet<i64> = HashSet::new();
            for repo in &repos {
                let default_branch = git::detect_default_branch(Path::new(&repo.path))
                    .unwrap_or_else(|_| "main".to_string());
                let Ok(worktrees) = state.db.list_worktrees(repo.id) else {
                    continue;
                };
                for w in worktrees {
                    // Orchestrators are repo-less scratch sessions — no branch,
                    // no PR. Skip them so we don't run `gh` against a non-repo.
                    if w.kind == "orchestrator" {
                        continue;
                    }
                    live.insert(w.id);
                    let status = compute(Path::new(&w.path), &default_branch);
                    if last.get(&w.id) != Some(&status) {
                        last.insert(w.id, status.clone());
                        let _ = app.emit(
                            "worktree:pr_status",
                            PrStatusEvent {
                                worktree_id: w.id,
                                status,
                            },
                        );
                    }
                }
            }
            // Forget worktrees that have been removed.
            last.retain(|k, _| live.contains(k));
        }
    });
}

/// Compute the PR status for one worktree. None means "no badge" (no PR and no
/// commits to submit, or any failure to reach `gh`).
pub fn compute(path: &Path, default_branch: &str) -> Option<PrStatus> {
    let out = match gh(
        path,
        &[
            "pr",
            "view",
            "--json",
            "state,isDraft,reviewDecision,statusCheckRollup,mergeable,number,url",
        ],
    ) {
        Some(o) => o,
        // gh missing / failed to spawn → degrade silently.
        None => return None,
    };

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no pull requests found") {
            // No PR for this branch — offer to submit only if there's something.
            return if commits_ahead_best(path, default_branch) > 0 {
                Some(PrStatus {
                    state: TaskState::ReadyToSubmit,
                    number: None,
                    url: None,
                })
            } else {
                None
            };
        }
        // Unauthenticated, non-GitHub remote, etc → no badge.
        return None;
    }

    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let number = v.get("number").and_then(Value::as_i64);
    let url = v.get("url").and_then(Value::as_str).map(String::from);
    let mk = |state: TaskState| {
        Some(PrStatus {
            state,
            number,
            url: url.clone(),
        })
    };

    match v.get("state").and_then(Value::as_str).unwrap_or("") {
        "MERGED" => return mk(TaskState::Merged),
        "CLOSED" => return mk(TaskState::Closed),
        _ => {}
    }
    if v.get("isDraft").and_then(Value::as_bool).unwrap_or(false) {
        return mk(TaskState::Draft);
    }

    let decision = v.get("reviewDecision").and_then(Value::as_str).unwrap_or("");
    let checks = summarize_checks(v.get("statusCheckRollup"));
    let mergeable = v.get("mergeable").and_then(Value::as_str).unwrap_or("");

    // "Your turn" signals win, ordered so the most actionable shows first.
    if decision == "CHANGES_REQUESTED" {
        return mk(TaskState::ChangesRequested);
    }
    if checks == CheckRollup::Failure {
        return mk(TaskState::CiFailed);
    }
    if mergeable == "CONFLICTING" {
        return mk(TaskState::Conflicts);
    }
    if unresolved_threads(path, url.as_deref(), number) > 0 {
        return mk(TaskState::CommentsToAddress);
    }
    if checks == CheckRollup::Pending {
        return mk(TaskState::MonitoringCi);
    }
    if decision == "APPROVED" && mergeable == "MERGEABLE" {
        return mk(TaskState::ReadyToMerge);
    }
    mk(TaskState::WaitingReview)
}

/// Count of unresolved review threads (human or bot) on the PR. 0 on any
/// failure. Needs the PR number + owner/repo, which we parse from the PR url to
/// avoid an extra `gh` round-trip.
fn unresolved_threads(path: &Path, url: Option<&str>, number: Option<i64>) -> i64 {
    let Some((owner, repo)) = url.and_then(parse_owner_repo) else {
        return 0;
    };
    let Some(number) = number else {
        return 0;
    };
    let query = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100){nodes{isResolved}}}}}";
    let out = gh(
        path,
        &[
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("owner={owner}"),
            "-F",
            &format!("repo={repo}"),
            "-F",
            &format!("number={number}"),
            "--jq",
            "[.data.repository.pullRequest.reviewThreads.nodes[]|select(.isResolved==false)]|length",
        ],
    );
    out.filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Parse "owner" and "repo" out of a github PR url like
/// `https://github.com/owner/repo/pull/123`.
fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let after = url.split("github.com/").nth(1)?;
    let mut parts = after.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Commits ahead of the base, trying `origin/<default>` then the local branch.
fn commits_ahead_best(path: &Path, default_branch: &str) -> usize {
    for base in [format!("origin/{default_branch}"), default_branch.to_string()] {
        if let Ok(n) = git::commits_ahead(path, &base) {
            return n;
        }
    }
    0
}

#[derive(PartialEq, Eq)]
enum CheckRollup {
    None,
    Pending,
    Success,
    Failure,
}

/// Collapse `gh`'s statusCheckRollup array (a mix of CheckRun + StatusContext
/// shapes) into one verdict: any failure dominates, then any pending.
fn summarize_checks(rollup: Option<&Value>) -> CheckRollup {
    let Some(arr) = rollup.and_then(Value::as_array) else {
        return CheckRollup::None;
    };
    let mut any_pending = false;
    let mut any_success = false;
    for item in arr {
        match classify_check(item) {
            CheckRollup::Failure => return CheckRollup::Failure,
            CheckRollup::Pending => any_pending = true,
            CheckRollup::Success => any_success = true,
            CheckRollup::None => {}
        }
    }
    if any_pending {
        CheckRollup::Pending
    } else if any_success {
        CheckRollup::Success
    } else {
        CheckRollup::None
    }
}

fn classify_check(item: &Value) -> CheckRollup {
    // StatusContext carries `state`; CheckRun carries `status` + `conclusion`.
    if let Some(state) = item.get("state").and_then(Value::as_str) {
        return match state {
            "SUCCESS" => CheckRollup::Success,
            "PENDING" | "EXPECTED" => CheckRollup::Pending,
            "FAILURE" | "ERROR" => CheckRollup::Failure,
            _ => CheckRollup::None,
        };
    }
    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "COMPLETED" {
        // QUEUED / IN_PROGRESS / WAITING / REQUESTED → still running.
        return CheckRollup::Pending;
    }
    match item.get("conclusion").and_then(Value::as_str).unwrap_or("") {
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckRollup::Success,
        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        | "STALE" => CheckRollup::Failure,
        _ => CheckRollup::None,
    }
}

/// Absolute path to the `gh` binary, resolved once. A bundled macOS app
/// launches from Finder/dock with a minimal PATH that omits Homebrew, so a bare
/// `Command::new("gh")` fails to spawn and every PR check silently returns no
/// badge. We find the real location instead.
fn gh_bin() -> &'static str {
    static GH: OnceLock<String> = OnceLock::new();
    GH.get_or_init(|| {
        for c in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"] {
            if Path::new(c).exists() {
                return c.to_string();
            }
        }
        // Last resort: ask the user's login shell where gh lives (mirrors how
        // monitor.rs reaches `claude` past the GUI's stripped PATH).
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        if let Ok(out) = Command::new(shell).args(["-ilc", "command -v gh"]).output() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return p;
            }
        }
        "gh".to_string()
    })
}

/// Run `gh` in a worktree. Returns None only when the process can't be spawned
/// (gh not installed); a non-zero exit comes back as a non-success `Output` so
/// callers can inspect stderr (e.g. "no pull requests found"). PATH is widened
/// so gh's own child `git` calls resolve under the bundled app's minimal PATH.
fn gh(path: &Path, args: &[&str]) -> Option<Output> {
    let augmented = match std::env::var("PATH") {
        Ok(p) => format!("/opt/homebrew/bin:/usr/local/bin:{p}"),
        Err(_) => "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".to_string(),
    };
    Command::new(gh_bin())
        .current_dir(path)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("PATH", augmented)
        .args(args)
        .output()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_owner_repo_from_pr_url() {
        assert_eq!(
            parse_owner_repo("https://github.com/acme/widgets/pull/42"),
            Some(("acme".into(), "widgets".into()))
        );
        assert_eq!(parse_owner_repo("https://example.com/x/y"), None);
    }

    #[test]
    fn failure_dominates_pending_and_success() {
        let rollup = json!([
            {"status": "COMPLETED", "conclusion": "SUCCESS"},
            {"status": "IN_PROGRESS"},
            {"state": "FAILURE"},
        ]);
        assert!(summarize_checks(Some(&rollup)) == CheckRollup::Failure);
    }

    #[test]
    fn pending_dominates_success() {
        let rollup = json!([
            {"status": "COMPLETED", "conclusion": "SUCCESS"},
            {"status": "QUEUED"},
        ]);
        assert!(summarize_checks(Some(&rollup)) == CheckRollup::Pending);
    }

    #[test]
    fn all_green_is_success() {
        let rollup = json!([
            {"status": "COMPLETED", "conclusion": "SUCCESS"},
            {"state": "SUCCESS"},
            {"status": "COMPLETED", "conclusion": "SKIPPED"},
        ]);
        assert!(summarize_checks(Some(&rollup)) == CheckRollup::Success);
    }

    #[test]
    fn empty_or_missing_rollup_is_none() {
        assert!(summarize_checks(Some(&json!([]))) == CheckRollup::None);
        assert!(summarize_checks(None) == CheckRollup::None);
    }
}
