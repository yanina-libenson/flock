//! Handoff between coding agents (Claude ↔ Codex) on the same worktree.
//!
//! A conversation can't move between agents — each has its own transcript
//! format and its own account — so the git branch carries the work and the
//! incoming agent gets a first prompt describing where things stand: the
//! original task, the last thing the user asked for, and the worktree's git
//! state. Used by `commands::worktree_set_agent`.

use crate::db::{Worktree, AGENT_CODEX};
use crate::transcript::{self, Msg};
use std::path::Path;

/// First line of every handoff prompt. Lets `user_requests` recognize a
/// previous handoff in a transcript so it's never mistaken for something the
/// user typed (e.g. when switching Codex → Claude → Codex).
const HANDOFF_HEADER: &str = "[Flock handoff]";

/// Caps so a huge pasted task or a sprawling diff can't blow up the command
/// line the prompt rides on.
const MAX_TASK_CHARS: usize = 6000;
const MAX_REQUEST_CHARS: usize = 3000;
const MAX_GIT_LINES: usize = 40;
const RECENT_COMMITS: usize = 10;

/// Everything the handoff prompt is built from. Gathered by `gather`, rendered
/// by `render` (pure, so it's unit-tested).
pub struct HandoffContext {
    pub original_task: Option<String>,
    pub last_request: Option<String>,
    pub git_status: String,
    pub diff_stat: String,
    pub recent_commits: String,
}

pub fn agent_label(agent: &str) -> &'static str {
    if agent == AGENT_CODEX {
        "Codex"
    } else {
        "Claude"
    }
}

/// The user's actual requests in a transcript, oldest first. Drops what isn't
/// something the user typed: Claude's injected meta turns (`<command-name>`,
/// `<system-reminder>`, local-command caveats, compaction summaries,
/// interrupt markers) and earlier Flock handoff prompts.
pub fn user_requests(msgs: &[Msg]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m.role == "user")
        .map(|m| m.text.trim())
        .filter(|t| {
            !t.is_empty()
                && !t.starts_with('<')
                && !t.starts_with(HANDOFF_HEADER)
                && !t.starts_with("[Request interrupted")
                && !t.starts_with("Caveat: The messages below")
                && !t.starts_with("This session is being continued from a previous conversation")
        })
        .map(str::to_string)
        .collect()
}

/// Read both agents' transcripts for the worktree plus its git state.
/// `from_agent` is the agent being stopped. The original task is the first
/// request in the Claude transcript (Claude always runs a worktree first; a
/// Codex transcript's first turn is usually a handoff), falling back to Codex's.
/// The last request comes from the outgoing agent, else the other one.
pub fn gather(w: &Worktree, env_vars: &[(String, String)], from_agent: &str) -> HandoffContext {
    let read = |path: Option<std::path::PathBuf>, codex: bool| -> Vec<String> {
        let Some(text) = path.and_then(|p| std::fs::read(p).ok()) else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&text);
        let msgs = if codex {
            transcript::parse_codex_messages(&text)
        } else {
            transcript::parse_messages(&text)
        };
        user_requests(&msgs)
    };
    let claude = read(
        transcript::session_file_for(&w.path, transcript::config_dir_from_env(env_vars)),
        false,
    );
    let codex = read(
        transcript::codex_session_for(&w.path, transcript::codex_home_from_env(env_vars))
            .map(|(p, _)| p),
        true,
    );

    let original_task = claude.first().or(codex.first()).cloned();
    let (outgoing, other) = if from_agent == AGENT_CODEX {
        (&codex, &claude)
    } else {
        (&claude, &codex)
    };
    let last_request = outgoing
        .last()
        .or(other.last())
        .cloned()
        .filter(|r| Some(r) != original_task.as_ref());

    let (git_status, diff_stat, recent_commits) =
        crate::git::handoff_snapshot(Path::new(&w.path), RECENT_COMMITS);
    HandoffContext {
        original_task,
        last_request,
        git_status,
        diff_stat,
        recent_commits,
    }
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n… (truncated)")
}

fn cap_lines(s: &str, max: usize) -> String {
    let lines: Vec<&str> = s.trim_end().lines().collect();
    if lines.is_empty() {
        return "(none)".to_string();
    }
    if lines.len() <= max {
        return lines.join("\n");
    }
    format!("{}\n… ({} more lines)", lines[..max].join("\n"), lines.len() - max)
}

/// The incoming agent's first prompt.
pub fn render(from_agent: &str, to_agent: &str, ctx: &HandoffContext) -> String {
    let from = agent_label(from_agent);
    let to = agent_label(to_agent);
    let mut out = format!(
        "{HANDOFF_HEADER} You ({to}) are taking over this worktree from {from}. \
The {from} session was stopped and its conversation can't be transferred — the git \
branch carries the work. Review the current state below (inspect the diff and commits \
yourself as needed), then continue the task from where it was left. Don't redo work \
that is already committed or in the working tree.\n"
    );
    out.push_str("\n## Original task\n");
    out.push_str(
        &ctx.original_task
            .as_deref()
            .map(|t| cap_chars(t, MAX_TASK_CHARS))
            .unwrap_or_else(|| "(not recorded)".to_string()),
    );
    out.push('\n');
    if let Some(last) = &ctx.last_request {
        out.push_str("\n## Most recent request\n");
        out.push_str(&cap_chars(last, MAX_REQUEST_CHARS));
        out.push('\n');
    }
    out.push_str("\n## Current state\n");
    out.push_str("git status --short --branch:\n");
    out.push_str(&cap_lines(&ctx.git_status, MAX_GIT_LINES));
    out.push_str("\n\nUncommitted changes (git diff --stat HEAD):\n");
    out.push_str(&cap_lines(&ctx.diff_stat, MAX_GIT_LINES));
    out.push_str(&format!("\n\nRecent commits (git log --oneline -n {RECENT_COMMITS}):\n"));
    out.push_str(&cap_lines(&ctx.recent_commits, RECENT_COMMITS));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> Msg {
        Msg {
            role: role.into(),
            text: text.into(),
        }
    }

    #[test]
    fn user_requests_drop_meta_turns_and_previous_handoffs() {
        let msgs = vec![
            msg("user", "Fix the checkout race"),
            msg("assistant", "On it."),
            msg("user", "<command-name>/model</command-name>"),
            msg("user", "Caveat: The messages below were generated by the user while running local commands."),
            msg("user", "[Request interrupted by user]"),
            msg("user", "[Flock handoff] You (Codex) are taking over…"),
            msg("user", "  also add a test  "),
        ];
        assert_eq!(
            user_requests(&msgs),
            vec!["Fix the checkout race".to_string(), "also add a test".to_string()]
        );
    }

    fn ctx() -> HandoffContext {
        HandoffContext {
            original_task: Some("Fix the checkout race".into()),
            last_request: Some("also add a test".into()),
            git_status: "## flock/fix-race\n M src/checkout.rs\n".into(),
            diff_stat: " src/checkout.rs | 12 ++++++------\n 1 file changed\n".into(),
            recent_commits: "abc1234 wip: lock the cart\n".into(),
        }
    }

    #[test]
    fn render_includes_task_last_request_and_git_state() {
        let p = render("claude", "codex", &ctx());
        assert!(p.starts_with("[Flock handoff] You (Codex) are taking over this worktree from Claude."));
        assert!(p.contains("## Original task\nFix the checkout race\n"));
        assert!(p.contains("## Most recent request\nalso add a test\n"));
        assert!(p.contains("## flock/fix-race\n M src/checkout.rs"));
        assert!(p.contains("src/checkout.rs | 12"));
        assert!(p.contains("abc1234 wip: lock the cart"));
        // The handoff never reads as a user request in the next transcript.
        assert!(user_requests(&[msg("user", &p)]).is_empty());
    }

    #[test]
    fn render_back_to_claude_and_missing_pieces() {
        let c = HandoffContext {
            original_task: None,
            last_request: None,
            git_status: String::new(),
            diff_stat: String::new(),
            recent_commits: String::new(),
        };
        let p = render("codex", "claude", &c);
        assert!(p.contains("You (Claude) are taking over this worktree from Codex."));
        assert!(p.contains("## Original task\n(not recorded)"));
        assert!(!p.contains("## Most recent request"));
        assert!(p.contains("Uncommitted changes (git diff --stat HEAD):\n(none)"));
    }

    #[test]
    fn long_inputs_are_capped() {
        let mut c = ctx();
        c.original_task = Some("x".repeat(MAX_TASK_CHARS + 500));
        c.git_status = (0..100).map(|i| format!(" M f{i}.rs\n")).collect();
        let p = render("claude", "codex", &c);
        assert!(p.contains("… (truncated)"));
        assert!(p.contains("… (60 more lines)"));
    }
}
