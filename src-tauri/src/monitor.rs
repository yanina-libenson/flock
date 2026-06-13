//! Background status monitor.
//!
//! Polls every Flock-owned tmux session via `capture-pane` and classifies each
//! agent as `working`, `idle`, or `needs_input`, emitting a `worktree:status`
//! event whenever a worktree's status changes. The frontend renders an
//! indicator and fires a native notification on the transition into
//! `needs_input`.
//!
//! Detection mirrors argus's two-signal model, adapted to the rendered screen
//! tmux hands us (no ANSI to strip, clean `\n`-separated lines):
//!   - **working** — the screen changed since the previous poll.
//!   - **needs_input** — Claude's blocking selection prompt (`❯ 1.`) is on
//!     screen, OR the screen is stable and the last transcript line is a
//!     question (`?`).
//!   - **idle** — stable, but no prompt and no trailing question.

use crate::pty;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// How often we capture every session. Doubles as the idle debounce: a screen
/// must stay unchanged across one full tick before it reads as idle /
/// needs_input, which keeps us from flagging an agent mid-render.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    Working,
    Idle,
    NeedsInput,
}

#[derive(Serialize, Clone)]
pub struct WorktreeStatusEvent {
    pub worktree_id: i64,
    pub status: WorktreeStatus,
}

/// Spawn iOS-style focusless monitoring. Runs for the life of the app on its
/// own thread; if tmux is absent every poll is a cheap no-op.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        // Last captured screen per worktree — the diff source for "stable".
        let mut prev: HashMap<i64, String> = HashMap::new();
        // Last emitted status per worktree — so we only emit on change.
        let mut last_status: HashMap<i64, WorktreeStatus> = HashMap::new();

        loop {
            std::thread::sleep(POLL_INTERVAL);

            let ids = pty::tmux_list_sessions();
            let live: HashSet<i64> = ids.iter().copied().collect();
            // A vanished session (claude exited, worktree removed) drops out of
            // tracking. The frontend clears its dot off the `pty:exit` event.
            prev.retain(|k, _| live.contains(k));
            last_status.retain(|k, _| live.contains(k));

            for id in ids {
                let Some(captured) = pty::tmux_capture_pane(id) else {
                    continue;
                };
                let status = detect_status(&captured, prev.get(&id).map(String::as_str));
                prev.insert(id, captured);

                if last_status.get(&id) != Some(&status) {
                    last_status.insert(id, status);
                    let _ = app.emit(
                        "worktree:status",
                        WorktreeStatusEvent {
                            worktree_id: id,
                            status,
                        },
                    );
                }
            }
        }
    });
}

/// Classify a freshly captured screen against the previous capture.
///
/// A blocking selection prompt (`❯ 1.`) reads as `needs_input` immediately —
/// it never streams past, so there's no value in waiting for stability, and a
/// fast notification is exactly what's wanted. The weaker trailing-question
/// signal is gated on stability so a `?` mid-stream doesn't false-fire.
pub fn detect_status(captured: &str, prev: Option<&str>) -> WorktreeStatus {
    if has_selection_prompt(captured) {
        return WorktreeStatus::NeedsInput;
    }
    let stable = prev == Some(captured);
    if stable {
        if ends_in_question(captured) {
            return WorktreeStatus::NeedsInput;
        }
        return WorktreeStatus::Idle;
    }
    WorktreeStatus::Working
}

/// Claude's numbered-selection UI: `❯` followed by optional spaces/tabs then
/// `1.`. The same widget renders permission prompts, AskUserQuestion overlays,
/// and plan-mode confirms, so matching the shape catches them all.
fn has_selection_prompt(screen: &str) -> bool {
    let mut rest = screen;
    while let Some(pos) = rest.find('❯') {
        let after = &rest[pos + '❯'.len_utf8()..];
        if after.trim_start_matches([' ', '\t']).starts_with("1.") {
            return true;
        }
        rest = after;
    }
    false
}

/// U+00A0 non-breaking space — the discriminator Claude's idle input line
/// renders after `❯`. Transcript text that merely contains `❯` (the selection
/// UI, shell prompts) uses a regular space.
const PROMPT_NBSP: &str = "❯\u{00a0}";

/// True when the last transcript line above Claude's input prompt ends in `?`.
///
/// Anchoring on the input prompt is what makes this usable: the hint lines
/// below it (`? for shortcuts`, `· ← for agents`) are excluded, and we only
/// inspect the genuine transcript above. The backward walk skips blank and
/// decoration lines (the spinner timing line, horizontal rules) to reach the
/// real last content line.
fn ends_in_question(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let mut anchor: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.contains(PROMPT_NBSP) || l.contains('╭') {
            anchor = Some(i);
        }
    }
    let Some(anchor) = anchor else {
        return false;
    };
    let mut i = anchor;
    while i > 0 {
        i -= 1;
        let trimmed = lines[i].trim_end();
        if trimmed.is_empty() || decoration_line(trimmed) {
            continue;
        }
        return matches!(trimmed.chars().last(), Some('?') | Some('？'));
    }
    false
}

/// UI chrome above the prompt that isn't transcript content: Claude's
/// spinner-glyph timing line ("✻ Brewed for 12s") or a box-drawing rule.
/// Transcript lines start with `⏺`/`⎿` or plain text, so neither check can
/// swallow a real question.
fn decoration_line(line: &str) -> bool {
    let line = line.trim_start();
    let mut chars = line.chars();
    match chars.next() {
        Some('·' | '✢' | '✳' | '✶' | '✻' | '✽') => true,
        Some(_) => line.chars().all(|c| c == '─' || c == '━' || c == '═'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_prompt_is_needs_input_even_when_changed() {
        let screen = "Pick one:\n❯ 1. Yes\n  2. No\n";
        // prev = None (just appeared) → still needs_input, no stability needed.
        assert_eq!(detect_status(screen, None), WorktreeStatus::NeedsInput);
    }

    #[test]
    fn selection_prompt_with_cursor_jump_no_space() {
        // The `❯3G` cursor-absolute path renders as `❯1.` after tmux strips it.
        assert!(has_selection_prompt("❯1. Approve"));
        assert!(has_selection_prompt("❯  1. Approve"));
        assert!(has_selection_prompt("❯\t1. Approve"));
    }

    #[test]
    fn trailing_question_needs_stability() {
        let screen = "⏺ Want me to ship it?\n\n❯\u{00a0}\n  ? for shortcuts\n";
        // Changed since last poll → still working (could be mid-stream).
        assert_eq!(detect_status(screen, Some("older")), WorktreeStatus::Working);
        // Stable → the question fires.
        assert_eq!(detect_status(screen, Some(screen)), WorktreeStatus::NeedsInput);
    }

    #[test]
    fn question_walks_past_spinner_and_blank_lines() {
        let screen = "⏺ Should I proceed?\n✻ Brewed for 12s\n\n❯\u{00a0}\n";
        assert!(ends_in_question(screen));
    }

    #[test]
    fn box_prompt_anchor_is_recognized() {
        let screen = "⏺ Ready to merge?\n╭──────────╮\n│ >        │\n╰──────────╯\n";
        assert!(ends_in_question(screen));
    }

    #[test]
    fn stable_non_question_screen_is_idle() {
        let screen = "⏺ Done. All tests pass.\n\n❯\u{00a0}\n  ? for shortcuts\n";
        assert_eq!(detect_status(screen, Some(screen)), WorktreeStatus::Idle);
    }

    #[test]
    fn changed_screen_is_working() {
        let screen = "⏺ Editing files...\n✻ Brewed for 3s\n";
        assert_eq!(detect_status(screen, Some("earlier output")), WorktreeStatus::Working);
    }

    #[test]
    fn no_prompt_anchor_means_no_question() {
        // A `?` in plain output with no input-prompt anchor must not fire.
        assert!(!ends_in_question("some log line ending in?\nmore output\n"));
    }

    #[test]
    fn hint_line_below_prompt_is_not_the_question() {
        // The `?` belongs to the hint line below the prompt, not a transcript
        // question — anchoring on the prompt excludes it.
        let screen = "⏺ All set.\n\n❯\u{00a0}\n  ? for shortcuts\n";
        assert!(!ends_in_question(screen));
    }
}
