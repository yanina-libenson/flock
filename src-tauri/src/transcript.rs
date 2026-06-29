//! Reads a worktree's Claude conversation from its session JSONL — the
//! structured transcript Claude Code writes per cwd at
//! `~/.claude/projects/<slug>/<session>.jsonl`. Powers the PWA "Reader" view:
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

/// Locate the active session file for a worktree: Claude encodes the cwd as a
/// slug under `~/.claude/projects`. The newest `.jsonl` in that dir is the live
/// session.
pub fn session_file_for(worktree_path: &str) -> Option<PathBuf> {
    let dir = dirs::home_dir()?
        .join(".claude/projects")
        .join(cwd_slug(worktree_path));
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

/// Flatten the JSONL into a clean conversation: user + assistant **text** only.
/// Thinking, tool calls, tool results, and metadata lines are dropped — the
/// Reader is for reading the conversation; the terminal stays for the details.
pub fn parse_messages(jsonl: &str) -> Vec<Msg> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let role = match v.get("type").and_then(|x| x.as_str()) {
            Some(r @ ("user" | "assistant")) => r.to_string(),
            _ => continue,
        };
        let text = extract_text(v.get("message").and_then(|m| m.get("content")));
        if !text.trim().is_empty() {
            out.push(Msg { role, text });
        }
    }
    out
}

fn extract_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter(|b| b.get("type").and_then(|x| x.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
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
}
