//! Child → orchestrator messages: the `notify_orchestrator` MCP tool
//! (`POST /api/notify`). A worktree spawned by an orchestrator (`parent_id`
//! set) sends a short report; Flock looks its orchestrator up from the row and
//! submits `[Flock · task #N "title" (repo) · kind] text` into that session via
//! `commands::deliver_input` (which resumes a dead orchestrator first).

use crate::commands::{deliver_input, DeliverError};
use crate::db::{Db, Worktree};
use crate::state::AppState;

/// What a child can report.
pub const KINDS: &[&str] = &["done", "blocked", "question", "info"];

#[derive(Debug)]
pub enum NotifyError {
    /// No worktree row for the caller.
    NotFound,
    /// The caller wasn't spawned by an orchestrator.
    NoParent,
    /// Bad `kind` or empty `text`.
    Invalid(String),
    /// Couldn't reach the orchestrator's session.
    Deliver(DeliverError),
}

/// The worktree and the id of the orchestrator it reports to.
fn resolve_parent(db: &Db, child_id: i64) -> Result<(Worktree, i64), NotifyError> {
    let child = db.get_worktree(child_id).map_err(|_| NotifyError::NotFound)?;
    let parent = child.parent_id.ok_or(NotifyError::NoParent)?;
    Ok((child, parent))
}

/// `[Flock · task #362 "title" (repo) · kind] text`, on one line — a newline
/// typed into the orchestrator would submit early.
fn format_notice(child: &Worktree, repo: &str, kind: &str, text: &str) -> String {
    let label = child.title.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or(&child.branch);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("[Flock · task #{} \"{}\" ({repo}) · {kind}] {text}", child.id, label.trim())
}

/// Send a child's report to its orchestrator. Blocking (tmux) — call from
/// spawn_blocking.
pub fn notify_parent(state: &AppState, child_id: i64, kind: &str, text: &str) -> Result<(), NotifyError> {
    if !KINDS.contains(&kind) {
        return Err(NotifyError::Invalid(format!("kind must be one of {}", KINDS.join(", "))));
    }
    if text.trim().is_empty() {
        return Err(NotifyError::Invalid("text is empty".into()));
    }
    let (child, parent) = resolve_parent(&state.db, child_id)?;
    let repo = state.db.get_repo(child.repo_id).map(|r| r.name).unwrap_or_default();
    let msg = format_notice(&child, &repo, kind, text);
    deliver_input(state, parent, true, &msg, true).map_err(NotifyError::Deliver)
}

/// Appended to an orchestrator-spawned child's system prompt (Claude
/// `--append-system-prompt`, Codex `developer_instructions`). None for a
/// worktree with no orchestrator.
pub fn child_system_prompt(parent_id: Option<i64>) -> Option<String> {
    parent_id?;
    Some(
        "You were spawned by a Flock orchestrator — another agent coordinating this work \
for the user. Keep it informed with the `notify_orchestrator` tool (Flock MCP; no id \
needed). When you finish your task, call it with kind \"done\" as your last step (a \
short summary, plus the PR URL if you opened one). Also call it with \"blocked\" when \
you're stuck, \"question\" when you need a decision (send it before you stop to ask), \
or \"info\" for something it must know now. Keep messages short and don't send \
progress updates."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Db {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "flock-notify-test-{}-{}.db",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let _ = std::fs::remove_file(&p);
        Db::open_at(&p).expect("open temp db")
    }

    /// An orchestrator, a child it spawned, and a user-created worktree.
    fn fleet(db: &Db) -> (i64, i64, i64) {
        let repo = db.insert_repo("acme", "/tmp/acme-notify").unwrap();
        let orch = db
            .insert_worktree(repo.id, "kyoto", "/tmp/orch-notify", None, "default", "orchestrator", None, None, None, None, "claude")
            .unwrap();
        let child = db
            .insert_worktree(repo.id, "flock/fix-login", "/tmp/child-notify", Some("Fix login"), "default", "worktree", Some(orch.id), None, None, None, "claude")
            .unwrap();
        let solo = db
            .insert_worktree(repo.id, "flock/solo", "/tmp/solo-notify", None, "default", "worktree", None, None, None, None, "claude")
            .unwrap();
        (orch.id, child.id, solo.id)
    }

    #[test]
    fn parent_comes_from_the_childs_row() {
        let db = temp_db();
        let (orch, child, solo) = fleet(&db);
        let (w, parent) = resolve_parent(&db, child).unwrap();
        assert_eq!((w.id, parent), (child, orch));
        assert!(matches!(resolve_parent(&db, solo), Err(NotifyError::NoParent)));
        assert!(matches!(resolve_parent(&db, orch), Err(NotifyError::NoParent)));
        assert!(matches!(resolve_parent(&db, 9999), Err(NotifyError::NotFound)));
    }

    #[test]
    fn notice_names_the_task_on_one_line() {
        let db = temp_db();
        let (_, child, solo) = fleet(&db);
        let w = db.get_worktree(child).unwrap();
        assert_eq!(
            format_notice(&w, "acme", "blocked", "  need the\nAPI key\n\n"),
            format!("[Flock · task #{child} \"Fix login\" (acme) · blocked] need the API key")
        );
        // No title → the branch.
        let s = db.get_worktree(solo).unwrap();
        assert_eq!(
            format_notice(&s, "acme", "done", "PR #4"),
            format!("[Flock · task #{solo} \"flock/solo\" (acme) · done] PR #4")
        );
    }

    #[test]
    fn child_instructions_only_with_an_orchestrator() {
        assert_eq!(child_system_prompt(None), None);
        let p = child_system_prompt(Some(3)).unwrap();
        assert!(p.contains("notify_orchestrator"));
        for kind in KINDS {
            assert!(p.contains(&format!("\"{kind}\"")), "{kind}");
        }
    }
}
