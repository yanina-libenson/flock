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
use crate::state::AppState;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// How often we capture every session. Doubles as the idle debounce: a screen
/// must stay unchanged across one full tick before it reads as idle /
/// needs_input, which keeps us from flagging an agent mid-render.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Minimum gap between phone push notifications for the same worktree.
const PUSH_COOLDOWN: Duration = Duration::from_secs(300);

/// How long a session must sit idle *at Claude's input prompt* before the
/// monitor hibernates it — kills the tmux session (and the resident `claude`,
/// freeing its RAM). Safe because reattaching resumes the on-disk transcript
/// (`claude --resume`, see `pty::attach`). Generous on purpose: hibernation also
/// requires the idle prompt to be on screen, so a quiet-but-working agent (a
/// long bash step with no output) is never reaped — only one that has handed
/// control back to the user and been left alone.
const HIBERNATE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Aggregate resident-memory budget across all live Flock `claude` sessions.
/// When their combined RSS exceeds this, the monitor reaps (hibernates) the
/// heaviest non-focused sessions until back under budget — even ones actively
/// `working`. This is the backstop the idle-prompt gate structurally can't
/// provide: a session stuck in a polling loop ("monitor the CI") is perpetually
/// `working`, never shows the idle prompt, and grows its conversation to
/// multiple GB; ten of those is how Flock reached ~30GB. Reaping is safe-ish —
/// the transcript is on disk, so reopening resumes via `claude --resume` — but
/// it loses the in-flight turn, so the budget is set high enough to only fire
/// under genuine pressure. Tune to the machine's RAM.
const RSS_BUDGET_KB: u64 = 12 * 1024 * 1024; // 12 GB

/// How often to run the memory-budget check. Memory grows slowly relative to the
/// 2s status poll, and the check shells out to `ps` over the whole process
/// table, so it gets its own slower cadence.
const RSS_CHECK_INTERVAL: Duration = Duration::from_secs(30);

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

#[derive(Serialize, Clone)]
pub struct WorktreeTitleEvent {
    pub worktree_id: i64,
    pub title: String,
}

#[derive(Serialize, Clone)]
pub struct WorktreeHibernatedEvent {
    pub worktree_id: i64,
    /// Why it was reaped: `"idle"` (parked at the prompt past the threshold) or
    /// `"memory"` (aggregate RSS budget). The frontend surfaces a banner for
    /// `"memory"` so the resumed session explains why it was killed.
    pub reason: String,
    /// Human detail for the banner, e.g. `"3.2 GB"` of RSS freed. None for idle.
    pub detail: Option<String>,
}

/// How long to wait for the one-shot `claude -p` title summarizer before
/// giving up and killing it. Generous — a cold `claude` start loads global
/// config; the call runs off the poll thread so latency only delays the title.
const TITLE_GEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn iOS-style focusless monitoring. Runs for the life of the app on its
/// own thread; if tmux is absent every poll is a cheap no-op.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        // Last captured screen per worktree — the diff source for "stable".
        let mut prev: HashMap<i64, String> = HashMap::new();
        // Last emitted status per worktree — so we only emit on change.
        let mut last_status: HashMap<i64, WorktreeStatus> = HashMap::new();
        // Worktrees we've already resolved a title for (or attempted) this run.
        // Prevents re-firing the summarizer; persistence across restarts is via
        // the DB title column (checked before generating).
        let mut titled: HashSet<i64> = HashSet::new();
        // Last push-notification time per worktree — caps phone pushes to one
        // per PUSH_COOLDOWN even if an agent flaps in/out of needs_input.
        let mut last_push: HashMap<i64, Instant> = HashMap::new();
        // When each session first went idle-at-prompt — the clock for
        // hibernation. Cleared the moment a session stops being idle.
        let mut idle_since: HashMap<i64, Instant> = HashMap::new();
        // Last time the aggregate-memory budget was checked (own slow cadence).
        let mut last_rss_check = Instant::now();

        // Cold-start baseline: prime each live session's last screen so the
        // first status tick diffs against a real previous frame, not an empty
        // one. Without this, every quiet session's first read is "changed →
        // working" (nothing to diff), then settles to idle a tick later — a
        // phantom working→idle that would wake the parent for a turn that
        // finished before launch (the #28 cold-start noise). Wakes are *also*
        // gated on `bootstrapped` below (flipped after the first full tick) as a
        // belt-and-braces guard, so only genuine post-launch edges ever wake.
        for id in pty::tmux_list_sessions() {
            if let Some(screen) = pty::tmux_capture_pane(id) {
                prev.insert(id, screen);
            }
        }
        let mut bootstrapped = false;

        loop {
            std::thread::sleep(POLL_INTERVAL);

            let ids = pty::tmux_list_sessions();
            let live: HashSet<i64> = ids.iter().copied().collect();
            // A vanished session (claude exited, worktree removed) drops out of
            // tracking. The frontend clears its dot off the `pty:exit` event.
            prev.retain(|k, _| live.contains(k));
            last_status.retain(|k, _| live.contains(k));
            titled.retain(|k| live.contains(k));
            last_push.retain(|k, _| live.contains(k));
            idle_since.retain(|k, _| live.contains(k));

            // The focused pane is never hibernated. Snapshot it once per tick.
            let active: Option<i64> = match app.try_state::<AppState>() {
                Some(s) => *s.active_worktree.lock().unwrap(),
                None => None,
            };
            let now = Instant::now();
            // Mirror the live set into the shared status map the REST API reads.
            if let Some(state) = app.try_state::<AppState>() {
                state.statuses.lock().unwrap().retain(|k, _| live.contains(k));
            }

            for id in ids {
                let Some(captured) = pty::tmux_capture_pane(id) else {
                    continue;
                };
                let status = detect_status(&captured, prev.get(&id).map(String::as_str));

                if last_status.get(&id) != Some(&status) {
                    let prev = last_status.get(&id).copied();
                    last_status.insert(id, status);
                    if let Some(state) = app.try_state::<AppState>() {
                        state.statuses.lock().unwrap().insert(id, status);
                    }
                    let _ = app.emit(
                        "worktree:status",
                        WorktreeStatusEvent {
                            worktree_id: id,
                            status,
                        },
                    );
                    // Push to the phone on entering needs_input, cooldown-gated.
                    if status == WorktreeStatus::NeedsInput {
                        let now = Instant::now();
                        let fresh = last_push
                            .get(&id)
                            .map(|t| now.duration_since(*t) >= PUSH_COOLDOWN)
                            .unwrap_or(true);
                        if fresh {
                            last_push.insert(id, now);
                            crate::api::notify_needs_input("Claude needs you".into(), worktree_label(&app, id));
                        }
                    }
                    // Wake the orchestrator parent on a child's attention edge
                    // (needs_input, or working→idle = turn done) so it acts
                    // without the human poking it. Fires once per edge (we're in
                    // the status-change branch). Skips a `working` parent — it'll
                    // see the child when its own turn ends; this is also what
                    // prevents wake loops (the trigger is always the child edge).
                    if let Some(state) = app.try_state::<AppState>() {
                        if let Ok(child) = state.db.get_worktree(id) {
                            let parent_alive =
                                child.parent_id.map(|p| live.contains(&p)).unwrap_or(false);
                            let parent_status =
                                child.parent_id.and_then(|p| last_status.get(&p).copied());
                            if let Some((parent_id, reason)) = wake_decision(
                                bootstrapped,
                                prev,
                                status,
                                child.parent_id,
                                parent_alive,
                                parent_status,
                            ) {
                                wake_parent(&app, parent_id, &child, reason);
                            }
                        }
                    }
                }

                maybe_generate_title(&app, &mut titled, id, &captured);

                // Idle-hibernation: a session parked at Claude's idle prompt
                // (not mid-task) past the threshold, and not the focused pane,
                // gets reaped to free its `claude` RAM. The conversation is on
                // disk, so reopening the pane resumes it.
                if status == WorktreeStatus::Idle && is_at_idle_prompt(&captured) {
                    let since = *idle_since.entry(id).or_insert(now);
                    if now.duration_since(since) >= HIBERNATE_AFTER && active != Some(id) {
                        hibernate(&app, id, "idle", None);
                        prev.remove(&id);
                        last_status.remove(&id);
                        titled.remove(&id);
                        last_push.remove(&id);
                        idle_since.remove(&id);
                        if let Some(state) = app.try_state::<AppState>() {
                            state.statuses.lock().unwrap().remove(&id);
                        }
                        continue; // session gone — don't cache its screen
                    }
                } else {
                    idle_since.remove(&id);
                }
                prev.insert(id, captured);
            }

            // The baseline is now established (this first tick seeded last_status
            // from screens primed before the loop). From here on, status edges
            // are genuine and may wake parents.
            bootstrapped = true;

            // Aggregate-memory backstop, on its own slow cadence: reap the
            // heaviest non-focused sessions when total RSS blows the budget.
            // Catches the looping-monitor leak the idle gate above can't.
            if now.duration_since(last_rss_check) >= RSS_CHECK_INTERVAL {
                last_rss_check = now;
                enforce_memory_budget(&app, &last_status, active);
            }
        }
    });
}

/// Reap the heaviest non-focused sessions when aggregate RSS exceeds
/// `RSS_BUDGET_KB`. Skips the focused pane and any session waiting on the user
/// (`needs_input`) — the two a user would notice vanishing; `working` and `idle`
/// sessions resume cleanly from the on-disk transcript. Cleanup of the reaped
/// ids from the monitor's per-session maps falls to the next tick's `retain`,
/// once the killed tmux session drops out of `tmux_list_sessions`.
fn enforce_memory_budget(
    app: &AppHandle,
    last_status: &HashMap<i64, WorktreeStatus>,
    active: Option<i64>,
) {
    let rss = pty::session_rss_kb();
    let total: u64 = rss.values().sum();
    for id in reap_targets(&rss, last_status, active, RSS_BUDGET_KB) {
        let kb = rss.get(&id).copied().unwrap_or(0);
        eprintln!(
            "flock: RSS budget exceeded ({}MB > {}MB) — hibernating worktree {} ({}MB)",
            total / 1024,
            RSS_BUDGET_KB / 1024,
            id,
            kb / 1024
        );
        hibernate(app, id, "memory", Some(human_gb(kb)));
    }
}

/// Format a KB count as a short human size for the hibernation banner.
fn human_gb(kb: u64) -> String {
    let gb = kb as f64 / 1024.0 / 1024.0;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{} MB", kb / 1024)
    }
}

/// Pick which sessions to reap to bring total RSS under `budget_kb`. Heaviest
/// first (frees the most per reap), excluding the focused pane and any session
/// waiting on the user. Returns empty when already under budget or when no
/// eligible session can close the gap. Pure — the side effects live in the
/// caller.
fn reap_targets(
    rss: &HashMap<i64, u64>,
    last_status: &HashMap<i64, WorktreeStatus>,
    active: Option<i64>,
    budget_kb: u64,
) -> Vec<i64> {
    let total: u64 = rss.values().sum();
    if total <= budget_kb {
        return Vec::new();
    }
    let mut candidates: Vec<(i64, u64)> = rss
        .iter()
        .filter(|(id, _)| active != Some(**id))
        .filter(|(id, _)| last_status.get(id) != Some(&WorktreeStatus::NeedsInput))
        .map(|(id, kb)| (*id, *kb))
        .collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    let mut targets = Vec::new();
    let mut freed = 0u64;
    for (id, kb) in candidates {
        if total - freed <= budget_kb {
            break;
        }
        targets.push(id);
        freed += kb;
    }
    targets
}

/// True when the screen shows Claude's idle input prompt (`❯` + NBSP) with no
/// blocking selection prompt and no trailing question — i.e. claude finished
/// its turn and is waiting for the user, as opposed to mid-task (a spinner or
/// streaming tool output, with no input prompt on screen). This is the
/// discriminator that makes hibernation safe: a busy-but-quiet agent never
/// shows the idle prompt, so it's never reaped.
fn is_at_idle_prompt(screen: &str) -> bool {
    screen.contains(PROMPT_NBSP) && !has_selection_prompt(screen) && !ends_in_question(screen)
}

/// Hibernate a worktree: kill its tmux session (and the `claude` inside it,
/// freeing memory), then tell the frontend so it can drop the pane to a dormant
/// tab. Reopening the pane reattaches with `claude --resume`, restoring the
/// conversation from disk. `reason`/`detail` ride along so the frontend can tell
/// the user why a session vanished (see WorktreeHibernatedEvent).
fn hibernate(app: &AppHandle, id: i64, reason: &str, detail: Option<String>) {
    if let Some(state) = app.try_state::<AppState>() {
        let _ = state.pty.kill(id);
    }
    pty::tmux_kill_session(id);
    let _ = app.emit(
        "worktree:hibernated",
        WorktreeHibernatedEvent {
            worktree_id: id,
            reason: reason.to_string(),
            detail,
        },
    );
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

/// Why a child's status edge warrants waking its orchestrator parent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WakeReason {
    /// The child is blocked waiting for input.
    NeedsInput,
    /// The child finished a turn (working → idle).
    FinishedTurn,
}

/// Decide whether a child's status edge should wake its parent, and which
/// parent. Pure — the side effects (resume + send) live in `wake_parent`.
///
/// Wakes on the child's edge INTO `needs_input`, or its `working → idle` (turn
/// done). Only wakes a parent that is idle or whose session is dead (resume it);
/// a `working` parent is mid-turn and will see the child when it finishes, and a
/// `needs_input` parent is itself waiting on the user. Returns `None` when the
/// child has no parent, the edge doesn't qualify, or the parent shouldn't be
/// disturbed — so the caller never wakes the parent off the parent's own edge,
/// which is what keeps this loop-free.
///
/// `bootstrapped` gates the whole thing: until the monitor has established its
/// baseline (first tick done), no edge wakes anyone — the statuses seen at
/// launch predate us (a child left idle/needs_input from a prior run, or the
/// `None → working → idle` settle when a quiet pane is first read), so waking on
/// them would fire for turns that finished before launch.
fn wake_decision(
    bootstrapped: bool,
    prev_child: Option<WorktreeStatus>,
    child: WorktreeStatus,
    parent_id: Option<i64>,
    parent_alive: bool,
    parent_status: Option<WorktreeStatus>,
) -> Option<(i64, WakeReason)> {
    if !bootstrapped {
        return None;
    }
    let parent_id = parent_id?;
    // Parent must be reachable-and-idle, or dead (we resume it). Leave a working
    // or needs_input parent alone.
    let parent_ready = !parent_alive || parent_status == Some(WorktreeStatus::Idle);
    if !parent_ready {
        return None;
    }
    let reason = match child {
        WorktreeStatus::NeedsInput if prev_child != Some(WorktreeStatus::NeedsInput) => {
            WakeReason::NeedsInput
        }
        WorktreeStatus::Idle if prev_child == Some(WorktreeStatus::Working) => {
            WakeReason::FinishedTurn
        }
        _ => return None,
    };
    Some((parent_id, reason))
}

/// Deliver a wake nudge to the orchestrator parent, resuming it first if its
/// session has died (reuses the resume-aware `commands::deliver_input`). Runs on
/// its own thread so the (possibly multi-second) resume + send never stalls the
/// monitor poll loop — same pattern as the title summarizer.
fn wake_parent(app: &AppHandle, parent_id: i64, child: &crate::db::Worktree, reason: WakeReason) {
    let label = child
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| child.branch.clone());
    let child_id = child.id;
    let state_word = match reason {
        WakeReason::NeedsInput => "needs your attention (needs_input)",
        WakeReason::FinishedTurn => "finished its turn (idle)",
    };
    let msg = format!(
        "🔔 Flock: your child task #{child_id} (\"{label}\") {state_word} — \
         check it with task_read/task_status."
    );
    let app = app.clone();
    std::thread::spawn(move || {
        if let Some(state) = app.try_state::<AppState>() {
            if let Err(e) = crate::commands::deliver_input(&state, parent_id, true, &msg, true) {
                eprintln!("flock: wake parent {parent_id} failed: {e:?}");
            }
        }
    });
}

/// Human label for a worktree's push body: its title if set, else the branch.
fn worktree_label(app: &AppHandle, id: i64) -> String {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(w) = state.db.get_worktree(id) {
            return w
                .title
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(w.branch);
        }
    }
    format!("worktree {id}")
}

/// Once per worktree, after the agent has actually responded, kick off a
/// background title summary. Gated so it fires exactly once: the `⏺` bullet
/// only appears after Claude produces a response (so we never summarize an
/// empty welcome screen), and an existing DB title short-circuits it on
/// restart. The generation itself runs on its own thread — a cold `claude -p`
/// can take seconds, and the 2s poll loop must not block on it.
fn maybe_generate_title(app: &AppHandle, titled: &mut HashSet<i64>, id: i64, screen: &str) {
    if titled.contains(&id) {
        return;
    }
    // No response yet → no task to title. Cheap gate before any DB/LLM work.
    if !screen.contains('⏺') {
        return;
    }
    let state = app.state::<AppState>();
    match state.db.get_worktree(id) {
        Ok(w) => {
            if w.title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                titled.insert(id); // already titled (e.g. set last run) — leave it
                return;
            }
        }
        // Session with no DB row (stale tmux session) — not ours to title.
        Err(_) => return,
    }

    // Mark attempted up front so subsequent polls don't spawn a second one
    // while this generation is in flight.
    titled.insert(id);
    let app = app.clone();
    let screen = screen.to_string();
    std::thread::spawn(move || {
        let Some(title) = generate_title(&screen) else {
            return;
        };
        let state = app.state::<AppState>();
        if state.db.update_worktree_title(id, &title).is_ok() {
            let _ = app.emit(
                "worktree:title",
                WorktreeTitleEvent {
                    worktree_id: id,
                    title,
                },
            );
        }
    });
}

/// Summarize a captured terminal screen into a short worktree title via a
/// one-shot headless `claude -p` call on the fast (haiku) model. Returns None
/// on any failure (claude missing, timeout, non-zero exit, empty reply) — the
/// caller just leaves the worktree untitled.
///
/// Runs through the user's interactive login shell — macOS launches the GUI
/// app with a minimal PATH, and `claude` needs the full PATH to find its
/// runtime. The prompt is piped via stdin (not an argv blob), so a multi-KB
/// screen needs no shell-escaping. cwd is a neutral temp dir so we don't load
/// the target project's CLAUDE.md / .mcp.json.
fn generate_title(screen: &str) -> Option<String> {
    let prompt = format!(
        "Below is a snapshot of a coding agent's terminal session. In 3 to 6 words, \
         write a short title describing what is being worked on. Reply with ONLY the \
         title — no quotes, no trailing punctuation, no preamble.\n\n---\n{screen}\n---"
    );
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut child = std::process::Command::new(shell)
        .args(["-i", "-l", "-c", "claude -p --model haiku"])
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // Feed the prompt, then close stdin so `claude -p` knows input is done.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    // Drain stdout on a side thread so a large reply can't deadlock the pipe
    // while we poll for exit.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + TITLE_GEN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let raw = rx.recv_timeout(Duration::from_secs(2)).ok()?;
                return sanitize_title(&raw);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => return None,
        }
    }
}

/// Clean a raw model reply into a title: first non-empty line, surrounding
/// quotes/backticks stripped, capped to 60 chars. None when nothing usable.
fn sanitize_title(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(60).collect())
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
    fn sanitize_title_strips_quotes_and_picks_first_line() {
        assert_eq!(
            sanitize_title("\"Fix the checkout race\"\n").as_deref(),
            Some("Fix the checkout race")
        );
        assert_eq!(
            sanitize_title("\n\n  Migrate auth to OAuth  \n").as_deref(),
            Some("Migrate auth to OAuth")
        );
        assert_eq!(sanitize_title("   \n  ").as_deref(), None);
        assert_eq!(sanitize_title("").as_deref(), None);
    }

    #[test]
    fn sanitize_title_caps_length() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_title(&long).unwrap().chars().count(), 60);
    }

    #[test]
    fn idle_prompt_gate_protects_busy_and_blocked_sessions() {
        // At the idle prompt, done → hibernatable.
        assert!(is_at_idle_prompt("⏺ Done. All tests pass.\n\n❯\u{00a0}\n  ? for shortcuts\n"));
        // Mid-task: spinner/output, no input prompt → NOT hibernatable.
        assert!(!is_at_idle_prompt("⏺ Running tests...\n✻ Brewed for 42s\n"));
        // Waiting on the user (selection prompt) → NOT hibernatable.
        assert!(!is_at_idle_prompt("Pick one:\n❯ 1. Yes\n  2. No\n"));
        // Trailing question (stable) → NOT hibernatable.
        assert!(!is_at_idle_prompt("⏺ Ship it?\n\n❯\u{00a0}\n"));
    }

    fn rss(pairs: &[(i64, u64)]) -> HashMap<i64, u64> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn under_budget_reaps_nothing() {
        let r = rss(&[(1, 1000), (2, 2000)]);
        assert!(reap_targets(&r, &HashMap::new(), None, 10_000).is_empty());
    }

    #[test]
    fn reaps_heaviest_first_until_under_budget() {
        // total 9000, budget 4000 → reaping the heaviest (4000) leaves 5000,
        // still over, so the next-heaviest (3000) joins → 2000, under budget.
        let r = rss(&[(1, 4000), (2, 3000), (3, 2000)]);
        assert_eq!(reap_targets(&r, &HashMap::new(), None, 4000), vec![1, 2]);
    }

    #[test]
    fn never_reaps_the_focused_pane_even_if_heaviest() {
        // worktree 1 is the biggest but focused → skipped; 2 then 3 get reaped.
        let r = rss(&[(1, 9000), (2, 3000), (3, 2000)]);
        assert_eq!(reap_targets(&r, &HashMap::new(), Some(1), 5000), vec![2, 3]);
    }

    #[test]
    fn never_reaps_a_session_waiting_on_the_user() {
        let r = rss(&[(1, 6000), (2, 5000)]);
        let mut st = HashMap::new();
        st.insert(1, WorktreeStatus::NeedsInput);
        // 1 is protected; only 2 is eligible (frees 5000, total 11000→6000>5000,
        // but no other candidate remains, so we reap what we can).
        assert_eq!(reap_targets(&r, &st, None, 5000), vec![2]);
    }

    #[test]
    fn child_edge_into_needs_input_wakes_idle_parent_once() {
        use WorktreeStatus::*;
        // working → needs_input with an idle parent → one wake.
        assert_eq!(
            wake_decision(true, Some(Working), NeedsInput, Some(5), true, Some(Idle)),
            Some((5, WakeReason::NeedsInput))
        );
        // Next tick is stable at needs_input (prev == needs_input) → no wake,
        // so a single edge produces exactly one wake.
        assert_eq!(
            wake_decision(true, Some(NeedsInput), NeedsInput, Some(5), true, Some(Idle)),
            None
        );
    }

    #[test]
    fn working_to_idle_edge_wakes_parent() {
        use WorktreeStatus::*;
        assert_eq!(
            wake_decision(true, Some(Working), Idle, Some(5), true, Some(Idle)),
            Some((5, WakeReason::FinishedTurn))
        );
        // idle → idle is not an edge (and the monitor wouldn't call it); even if
        // called, a stable idle child must not wake.
        assert_eq!(
            wake_decision(true, Some(Idle), Idle, Some(5), true, Some(Idle)),
            None
        );
        // None → idle (first sighting parked at idle) is not a finished turn.
        assert_eq!(wake_decision(true, None, Idle, Some(5), true, Some(Idle)), None);
    }

    #[test]
    fn child_without_parent_never_wakes() {
        use WorktreeStatus::*;
        assert_eq!(
            wake_decision(true, Some(Working), NeedsInput, None, false, None),
            None
        );
        assert_eq!(wake_decision(true, Some(Working), Idle, None, false, None), None);
    }

    #[test]
    fn working_parent_is_not_woken() {
        use WorktreeStatus::*;
        // Parent mid-turn → left alone (it'll see the child when it finishes).
        assert_eq!(
            wake_decision(true, Some(Working), NeedsInput, Some(5), true, Some(Working)),
            None
        );
        assert_eq!(
            wake_decision(true, Some(Working), Idle, Some(5), true, Some(Working)),
            None
        );
        // A needs_input parent (itself waiting on the user) is also left alone.
        assert_eq!(
            wake_decision(true, Some(Working), NeedsInput, Some(5), true, Some(NeedsInput)),
            None
        );
    }

    #[test]
    fn dead_parent_session_is_woken_for_resume() {
        use WorktreeStatus::*;
        // Parent not in the live set (hibernated/reaped) → wake so deliver_input
        // resumes it. No tracked status for a dead parent.
        assert_eq!(
            wake_decision(true, Some(Working), NeedsInput, Some(5), false, None),
            Some((5, WakeReason::NeedsInput))
        );
    }

    #[test]
    fn cold_start_first_tick_never_wakes() {
        use WorktreeStatus::*;
        // During bootstrap (bootstrapped = false), no transition observed at
        // launch wakes the parent — including the None → working → idle settle
        // a quiet pane shows on first read, and a child found already parked in
        // needs_input from a prior run. Parent idle and "wakeable" in every case
        // to prove it's the bootstrap gate doing the suppressing, not the
        // parent-state guard.
        assert_eq!(
            wake_decision(false, None, Working, Some(5), true, Some(Idle)),
            None
        );
        assert_eq!(
            wake_decision(false, Some(Working), Idle, Some(5), true, Some(Idle)),
            None
        );
        assert_eq!(
            wake_decision(false, None, NeedsInput, Some(5), true, Some(Idle)),
            None
        );
        assert_eq!(
            wake_decision(false, Some(Working), NeedsInput, Some(5), false, None),
            None
        );
    }

    #[test]
    fn genuine_transition_after_bootstrap_wakes() {
        use WorktreeStatus::*;
        // The same working → idle edge that was suppressed at launch DOES wake
        // once the baseline is established.
        assert_eq!(
            wake_decision(true, Some(Working), Idle, Some(5), true, Some(Idle)),
            Some((5, WakeReason::FinishedTurn))
        );
    }

    #[test]
    fn hint_line_below_prompt_is_not_the_question() {
        // The `?` belongs to the hint line below the prompt, not a transcript
        // question — anchoring on the prompt excludes it.
        let screen = "⏺ All set.\n\n❯\u{00a0}\n  ? for shortcuts\n";
        assert!(!ends_in_question(screen));
    }
}
