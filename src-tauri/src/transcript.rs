//! Reads a worktree's Claude conversation from its session JSONL — the
//! structured transcript Claude Code writes per cwd at
//! `<config>/projects/<slug>/<session>.jsonl`, where `<config>` is
//! `$CLAUDE_CONFIG_DIR` when set, else `~/.claude`. Flock binds a distinct
//! `CLAUDE_CONFIG_DIR` per env profile (e.g. a separate Claude for the Personal
//! folder), so both resume and the Reader must look under the session's own
//! config dir — not a hardcoded `~/.claude`. Powers the Reader feed (`/api/worktrees/:id/transcript`):
//! a clean, reflowable chat that's fully decoupled from the terminal (read-only
//! file access — never touches the live tmux session or its width).

use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Msg {
    pub role: String,
    pub text: String,
}

/// Claude's cwd → project-dir slug: every `/`, `.`, and whitespace char becomes
/// `-` (char-for-char). The whitespace case matters for paths like the
/// orchestrator scratch dir under `~/Library/Application Support/…` — without
/// it the slug keeps the space and we'd miss the transcript entirely (so resume
/// and the Reader silently break for any cwd with a space).
pub fn cwd_slug(path: &str) -> String {
    path.chars()
        .map(|c| if c == '/' || c == '.' || c.is_whitespace() { '-' } else { c })
        .collect()
}

/// The `CLAUDE_CONFIG_DIR` from a session's resolved env vars, if any. Flock
/// runs separate Claude configs per folder (work vs the Personal folder), so a
/// worktree's transcripts can live under a non-default root; callers pass this
/// to `session_file_for` / `latest_session_id` so resume and the Reader look in
/// the right place.
pub fn config_dir_from_env(env_vars: &[(String, String)]) -> Option<&str> {
    env_vars
        .iter()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| v.as_str())
}

/// The `projects` root for a session: `<CLAUDE_CONFIG_DIR>/projects` when set,
/// else `~/.claude/projects`. Mirrors how Claude Code chooses where to write
/// transcripts.
fn projects_dir(config_dir: Option<&str>) -> Option<PathBuf> {
    match config_dir {
        Some(d) => Some(PathBuf::from(d).join("projects")),
        None => Some(dirs::home_dir()?.join(".claude/projects")),
    }
}

/// Locate the active session file for a worktree: Claude encodes the cwd as a
/// slug under `<config>/projects`. The newest `.jsonl` in that dir is the live
/// session. `config_dir` is the session's `CLAUDE_CONFIG_DIR` (see
/// `config_dir_from_env`); `None` falls back to `~/.claude`.
pub fn session_file_for(worktree_path: &str, config_dir: Option<&str>) -> Option<PathBuf> {
    let dir = projects_dir(config_dir)?.join(cwd_slug(worktree_path));
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(mt) = entry.metadata().ok().and_then(|m| m.modified().ok()) {
            if best.as_ref().is_none_or(|(b, _)| mt > *b) {
                best = Some((mt, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Flatten the JSONL into a clean conversation: user + assistant **text**, plus
/// `AskUserQuestion` calls rendered as text (agents often put a whole checkpoint
/// in one, and an orchestrator must see what a child is blocked on). Thinking,
/// other tool calls, tool results, and metadata lines are dropped — the Reader
/// is for reading the conversation; the terminal stays for the details.
pub fn parse_messages(jsonl: &str) -> Vec<Msg> {
    let values: Vec<serde_json::Value> = jsonl
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    // tool_use ids that already got a tool_result, i.e. the user answered.
    let answered: std::collections::HashSet<&str> = values
        .iter()
        .filter_map(|v| v.get("message")?.get("content")?.as_array())
        .flatten()
        .filter(|b| b.get("type").and_then(|x| x.as_str()) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id")?.as_str())
        .collect();
    let mut out = Vec::new();
    for v in &values {
        let role = match v.get("type").and_then(|x| x.as_str()) {
            Some(r @ ("user" | "assistant")) => r.to_string(),
            _ => continue,
        };
        let text = extract_text(v.get("message").and_then(|m| m.get("content")), &answered);
        if !text.trim().is_empty() {
            out.push(Msg { role, text });
        }
    }
    out
}

// ---------- Codex ----------
//
// Codex CLI writes one rollout JSONL per session under
// `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` (CODEX_HOME
// defaults to `~/.codex`). Unlike Claude, the path doesn't encode the cwd —
// the first line is a `session_meta` record carrying the session `id` and its
// `cwd`. So a worktree is tied to its Codex session by matching that `cwd`
// against the worktree path, newest rollout first: the same "newest session
// for this cwd is the live one" rule the Claude side uses.

/// `CODEX_HOME` from a session's resolved env vars, if a profile sets one.
pub fn codex_home_from_env(env_vars: &[(String, String)]) -> Option<&str> {
    env_vars
        .iter()
        .find(|(k, _)| k == "CODEX_HOME")
        .map(|(_, v)| v.as_str())
}

fn codex_sessions_dir(codex_home: Option<&str>) -> Option<PathBuf> {
    match codex_home {
        Some(d) => Some(PathBuf::from(d).join("sessions")),
        None => Some(dirs::home_dir()?.join(".codex/sessions")),
    }
}

/// The interactive session a rollout's first (`session_meta`) line describes,
/// as `(id, cwd)`. None for anything else — including non-interactive
/// `codex exec` runs and sub-agent threads, which can share the worktree's cwd
/// (e.g. a Claude session shelling out to `codex exec` for a review) but are
/// never the session to resume.
fn codex_session_meta(first_line: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(first_line).ok()?;
    if v.get("type").and_then(|x| x.as_str()) != Some("session_meta") {
        return None;
    }
    let p = v.get("payload")?;
    if p.get("source").and_then(|x| x.as_str()) == Some("exec") {
        return None;
    }
    if let Some(ts) = p.get("thread_source").and_then(|x| x.as_str()) {
        if ts != "user" {
            return None;
        }
    }
    let id = p.get("id").and_then(|x| x.as_str())?.to_string();
    let cwd = p.get("cwd").and_then(|x| x.as_str())?.to_string();
    Some((id, cwd))
}

fn same_dir(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The newest interactive Codex session for a worktree, as `(rollout file,
/// session id)`, or None if Codex has never run there. `codex_home` is the
/// session's `CODEX_HOME` (see `codex_home_from_env`); None → `~/.codex`.
pub fn codex_session_for(
    worktree_path: &str,
    codex_home: Option<&str>,
) -> Option<(PathBuf, String)> {
    use std::io::BufRead;
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut stack = vec![codex_sessions_dir(codex_home)?];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Some(mt) = entry.metadata().ok().and_then(|m| m.modified().ok()) {
                    files.push((mt, path));
                }
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, path) in files {
        let Ok(f) = std::fs::File::open(&path) else {
            continue;
        };
        let mut first = String::new();
        if std::io::BufReader::new(f).read_line(&mut first).is_err() {
            continue;
        }
        if let Some((id, cwd)) = codex_session_meta(&first) {
            if same_dir(&cwd, worktree_path) {
                return Some((path, id));
            }
        }
    }
    None
}

/// Flatten a Codex rollout into the same clean user/assistant text the Claude
/// Reader shows. Reads the `event_msg` stream rather than the raw
/// `response_item`s, because the latter also carry injected context (AGENTS.md,
/// environment blocks) as "user" messages. Handles both rollout generations:
/// `user_message` / `agent_message` events (older CLIs) and `item_completed`
/// events wrapping `UserMessage` / `AgentMessage` items (current CLIs).
pub fn parse_codex_messages(jsonl: &str) -> Vec<Msg> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("event_msg") {
            continue;
        }
        let Some(p) = v.get("payload") else {
            continue;
        };
        let (role, text) = match p.get("type").and_then(|x| x.as_str()) {
            Some("user_message") => ("user", p.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string()),
            Some("agent_message") => ("assistant", p.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string()),
            Some("item_completed") => {
                let item = p.get("item");
                let role = match item.and_then(|i| i.get("type")).and_then(|x| x.as_str()) {
                    Some("UserMessage") => "user",
                    Some("AgentMessage") => "assistant",
                    _ => continue,
                };
                let text = item
                    .and_then(|i| i.get("content"))
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                (role, text)
            }
            _ => continue,
        };
        if !text.trim().is_empty() {
            out.push(Msg { role: role.to_string(), text });
        }
    }
    out
}

fn extract_text(
    content: Option<&serde_json::Value>,
    answered: &std::collections::HashSet<&str>,
) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| match b.get("type").and_then(|x| x.as_str()) {
                Some("text") => b.get("text").and_then(|x| x.as_str()).map(str::to_string),
                Some("tool_use") if b.get("name").and_then(|x| x.as_str()) == Some("AskUserQuestion") => {
                    let pending = !b
                        .get("id")
                        .and_then(|x| x.as_str())
                        .is_some_and(|id| answered.contains(id));
                    Some(render_ask_user_question(b.get("input"), pending))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// An `AskUserQuestion` input as readable text: each question, then its options
/// as a numbered list. `pending` marks one the user hasn't answered yet.
fn render_ask_user_question(input: Option<&serde_json::Value>, pending: bool) -> String {
    let mut lines = Vec::new();
    if pending {
        lines.push("[waiting for answer]".to_string());
    }
    let questions = input
        .and_then(|i| i.get("questions"))
        .and_then(|q| q.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    for q in questions {
        let Some(text) = q.get("question").and_then(|x| x.as_str()) else {
            continue;
        };
        let multi = q.get("multiSelect").and_then(|x| x.as_bool()) == Some(true);
        lines.push(if multi { format!("{text} (multi-select)") } else { text.to_string() });
        let options = q.get("options").and_then(|o| o.as_array()).map(Vec::as_slice).unwrap_or_default();
        for (i, o) in options.iter().enumerate() {
            let label = o.get("label").and_then(|x| x.as_str()).unwrap_or("");
            match o.get("description").and_then(|x| x.as_str()).filter(|d| !d.is_empty()) {
                Some(d) => lines.push(format!("{}. {label}: {d}", i + 1)),
                None => lines.push(format!("{}. {label}", i + 1)),
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_and_array_content() {
        let jsonl = r#"
{"type":"user","message":{"role":"user","content":"hola, arreglá el bug"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Dale, lo veo."}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"output"}]}}
{"type":"ai-title","title":"fix bug"}
"#;
        let msgs = parse_messages(jsonl);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], Msg { role: "user".into(), text: "hola, arreglá el bug".into() });
        assert_eq!(msgs[1], Msg { role: "assistant".into(), text: "Dale, lo veo.".into() });
    }

    const ASK: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"AskUserQuestion","input":{"questions":[{"question":"Which plan?","header":"Plan","multiSelect":false,"options":[{"label":"A","description":"fast"},{"label":"B","description":"safe"}]},{"question":"Which envs?","header":"Env","multiSelect":true,"options":[{"label":"dev","description":""}]}]}}]}}"#;
    const ANSWER: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"A"}]}}"#;

    #[test]
    fn renders_pending_ask_user_question() {
        let msgs = parse_messages(ASK);
        assert_eq!(
            msgs,
            vec![Msg {
                role: "assistant".into(),
                text: "[waiting for answer]\nWhich plan?\n1. A: fast\n2. B: safe\nWhich envs? (multi-select)\n1. dev".into()
            }]
        );
    }

    #[test]
    fn answered_ask_user_question_is_not_pending() {
        let msgs = parse_messages(&format!("{ASK}\n{ANSWER}"));
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text.starts_with("Which plan?\n1. A: fast"));
        assert!(!msgs[0].text.contains("waiting"));
    }

    #[test]
    fn skips_malformed_and_empty() {
        let jsonl = "not json\n\n{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n";
        assert!(parse_messages(jsonl).is_empty());
    }

    #[test]
    fn slug_encoding() {
        // Sanity: the cwd→slug transform Claude uses.
        assert_eq!(
            super::cwd_slug("/Users/y/Code/work/.flock-worktrees/x"),
            "-Users-y-Code-work--flock-worktrees-x"
        );
        // Spaces become dashes too — the orchestrator scratch dir lives under
        // "Application Support", and resume/Reader depend on this matching.
        assert_eq!(
            super::cwd_slug("/Users/y/Library/Application Support/Flock/orchestrators/kyoto"),
            "-Users-y-Library-Application-Support-Flock-orchestrators-kyoto"
        );
    }

    #[test]
    fn config_dir_from_env_reads_claude_config_dir() {
        let env = vec![
            ("GH_CONFIG_DIR".to_string(), "/x/gh".to_string()),
            ("CLAUDE_CONFIG_DIR".to_string(), "/Users/y/.claude-personal".to_string()),
        ];
        assert_eq!(config_dir_from_env(&env), Some("/Users/y/.claude-personal"));
        assert_eq!(config_dir_from_env(&[]), None);
    }

    #[test]
    fn parses_current_codex_rollout_events() {
        // Trimmed from a real codex-cli 0.160 rollout. The injected AGENTS.md
        // "user" response_item and the tool calls must not show up.
        let jsonl = r##"
{"timestamp":"t","type":"session_meta","payload":{"id":"01a1","cwd":"/w","source":"cli","thread_source":"user"}}
{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n..."}]}}
{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"List the files"}]}}
{"timestamp":"t","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"List the files","text_elements":[]}]}}}
{"timestamp":"t","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"I'll list them."}],"phase":"commentary"}}}
{"timestamp":"t","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","ls"]}}}
{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a.txt"}]}}
{"timestamp":"t","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"a.txt"}],"phase":"final_answer"}}}
{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"a.txt"}}
"##;
        let msgs = parse_codex_messages(jsonl);
        assert_eq!(
            msgs,
            vec![
                Msg { role: "user".into(), text: "List the files".into() },
                Msg { role: "assistant".into(), text: "I'll list them.".into() },
                Msg { role: "assistant".into(), text: "a.txt".into() },
            ]
        );
    }

    #[test]
    fn parses_legacy_codex_rollout_events() {
        let jsonl = r#"
{"type":"event_msg","payload":{"type":"user_message","message":"implement it","images":[]}}
{"type":"event_msg","payload":{"type":"agent_message","message":"Done."}}
{"type":"event_msg","payload":{"type":"token_count","info":null}}
not json
"#;
        let msgs = parse_codex_messages(jsonl);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], Msg { role: "user".into(), text: "implement it".into() });
        assert_eq!(msgs[1], Msg { role: "assistant".into(), text: "Done.".into() });
    }

    #[test]
    fn codex_session_meta_skips_exec_and_subagent_sessions() {
        let tui = r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/w","source":"cli","thread_source":"user"}}"#;
        assert_eq!(codex_session_meta(tui), Some(("abc".into(), "/w".into())));
        // Older rollouts carry no thread_source at all.
        let old = r#"{"type":"session_meta","payload":{"id":"old","cwd":"/w","source":"vscode"}}"#;
        assert_eq!(codex_session_meta(old), Some(("old".into(), "/w".into())));
        let exec = r#"{"type":"session_meta","payload":{"id":"x","cwd":"/w","source":"exec","thread_source":"user"}}"#;
        assert_eq!(codex_session_meta(exec), None);
        let sub = r#"{"type":"session_meta","payload":{"id":"y","cwd":"/w","source":"cli","thread_source":"subagent"}}"#;
        assert_eq!(codex_session_meta(sub), None);
        assert_eq!(codex_session_meta(r#"{"type":"event_msg","payload":{}}"#), None);
    }

    #[test]
    fn codex_session_for_picks_newest_interactive_rollout_for_cwd() {
        let home = std::env::temp_dir().join(format!("flock-codex-home-{}", std::process::id()));
        let day = home.join("sessions/2026/10/05");
        std::fs::create_dir_all(&day).unwrap();
        let meta = |id: &str, cwd: &str, source: &str| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"source\":\"{source}\",\"thread_source\":\"user\"}}}}\n"
            )
        };
        let write = |name: &str, body: String| {
            std::fs::write(day.join(name), body).unwrap();
            // Distinct mtimes so "newest" is well defined.
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        write("rollout-a.jsonl", meta("old-session", "/work/wt", "cli"));
        write("rollout-b.jsonl", meta("other-cwd", "/work/other", "cli"));
        write("rollout-c.jsonl", meta("new-session", "/work/wt/", "cli"));
        // Newest of all, same cwd, but a `codex exec` run → skipped.
        write("rollout-d.jsonl", meta("exec-run", "/work/wt", "exec"));

        let home_s = home.to_string_lossy().into_owned();
        let (file, id) = codex_session_for("/work/wt", Some(&home_s)).unwrap();
        assert_eq!(id, "new-session");
        assert!(file.ends_with("rollout-c.jsonl"));
        assert_eq!(codex_session_for("/work/none", Some(&home_s)), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn projects_dir_honors_config_dir() {
        // A set CLAUDE_CONFIG_DIR roots transcripts under <dir>/projects — the
        // work-vs-Personal split that broke resume for Personal worktrees.
        assert_eq!(
            projects_dir(Some("/Users/y/.claude-personal")),
            Some(PathBuf::from("/Users/y/.claude-personal/projects"))
        );
        // No override falls back to ~/.claude/projects.
        assert_eq!(
            projects_dir(None),
            dirs::home_dir().map(|h| h.join(".claude/projects"))
        );
    }
}
