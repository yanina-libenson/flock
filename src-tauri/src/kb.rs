//! Knowledge base: an Obsidian vault indexed into SQLite FTS5 and served over
//! MCP — Flock's port of argus's third pillar.
//!
//! The vault on disk is the source of truth; the FTS index (see `db.rs`) is
//! rebuilt from it. Agents read/write via the `kb_*` MCP tools (treating it as
//! durable cross-session memory), and a file watcher keeps the index fresh when
//! notes are edited directly in Obsidian. There is no automatic session
//! capture — like argus, the graph grows because agents choose to write.

use crate::db::Db;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

// ---------- config (vault path) ----------

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct KbConfig {
    #[serde(default)]
    pub vault_path: Option<String>,
}

fn config_path() -> std::io::Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| std::io::Error::other("no data local dir"))?
        .join("Flock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("kb.json"))
}

pub fn load_config() -> KbConfig {
    config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &KbConfig) -> std::io::Result<()> {
    let path = config_path()?;
    let json = serde_json::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

pub fn vault_path() -> Option<String> {
    load_config()
        .vault_path
        .filter(|v| !v.trim().is_empty())
}

// ---------- markdown parsing ----------

struct Parsed {
    title: String,
    body: String,
    /// Space-separated tags (stored that way for FTS5).
    tags: String,
    word_count: i64,
}

/// Parse a note: split YAML front-matter, derive the title (front-matter →
/// first H1 → filename stem), collect tags, count words in the body.
fn parse_document(rel_path: &str, content: &str) -> Parsed {
    let mut iter = content.lines().peekable();
    let mut fm_lines: Vec<&str> = Vec::new();
    let mut has_fm = false;
    let body: String = if iter.peek().map(|l| l.trim_end()) == Some("---") {
        iter.next(); // opening ---
        for line in iter.by_ref() {
            if line.trim_end() == "---" {
                has_fm = true;
                break;
            }
            fm_lines.push(line);
        }
        if has_fm {
            iter.collect::<Vec<_>>().join("\n")
        } else {
            content.to_string()
        }
    } else {
        content.to_string()
    };

    let (mut title, tags) = if has_fm {
        parse_frontmatter(&fm_lines)
    } else {
        (None, String::new())
    };
    if title.is_none() {
        title = first_h1(&body);
    }
    let title = title.unwrap_or_else(|| filename_stem(rel_path));
    let word_count = body.split_whitespace().count() as i64;
    Parsed {
        title,
        body,
        tags,
        word_count,
    }
}

fn parse_frontmatter(lines: &[&str]) -> (Option<String>, String) {
    let mut title: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut in_tags_list = false;
    for line in lines {
        let trimmed = line.trim();
        if in_tags_list {
            if let Some(rest) = trimmed.strip_prefix('-') {
                let t = clean_tag(rest);
                if !t.is_empty() {
                    tags.push(t);
                }
                continue;
            }
            in_tags_list = false; // a non-list line ends the block
        }
        if let Some(rest) = trimmed.strip_prefix("title:") {
            let t = unquote(rest.trim());
            if !t.is_empty() {
                title = Some(t.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("tags:") {
            let v = rest.trim();
            if v.is_empty() {
                in_tags_list = true; // YAML block list follows
            } else {
                let v = v.trim_start_matches('[').trim_end_matches(']');
                for t in v.split(',') {
                    let t = clean_tag(t);
                    if !t.is_empty() {
                        tags.push(t);
                    }
                }
            }
        }
    }
    (title.filter(|t| !t.is_empty()), tags.join(" "))
}

fn clean_tag(s: &str) -> String {
    unquote(s.trim()).trim_start_matches('#').trim().to_string()
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn first_h1(body: &str) -> Option<String> {
    for line in body.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("# ") {
            let h = rest.trim();
            if !h.is_empty() {
                return Some(h.to_string());
            }
        }
    }
    None
}

fn filename_stem(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.strip_suffix(".md")
        .or_else(|| name.strip_suffix(".MD"))
        .unwrap_or(name)
        .to_string()
}

// ---------- query / path sanitizing ----------

/// Turn arbitrary user/agent text into a safe FTS5 MATCH expression: split on
/// non-alphanumerics, quote each token (so FTS5 specials can't break parsing),
/// AND them together. Empty when the query has no usable tokens.
pub fn sanitize_query(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize an agent-supplied vault path: relative, no traversal, ends in
/// `.md`. Rejects absolute paths and `..` so a write can't escape the vault.
fn sanitize_rel_path(p: &str) -> AppResult<String> {
    let p = p.trim().trim_start_matches('/');
    if p.is_empty() || p.split('/').any(|c| c == ".." || c == "." || c.is_empty()) {
        return Err(AppError::msg(format!("invalid note path: {p:?}")));
    }
    Ok(if p.to_lowercase().ends_with(".md") {
        p.to_string()
    } else {
        format!("{p}.md")
    })
}

// ---------- indexing ----------

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_of(p: &Path) -> i64 {
    p.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_markdown(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn is_hidden(e: &walkdir::DirEntry) -> bool {
    e.depth() > 0
        && e.file_name()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
}

fn has_hidden_component(rel: &str) -> bool {
    rel.split('/').any(|c| c.starts_with('.'))
}

/// Full/incremental re-scan of the vault: index new or mtime-changed `.md`
/// files, skip unchanged ones, and prune docs whose file was deleted. Returns
/// the number of (re)indexed files.
pub fn reindex(db: &Db, vault: &str) -> AppResult<usize> {
    let root = Path::new(vault);
    if !root.is_dir() {
        return Err(AppError::msg(format!("vault is not a directory: {vault}")));
    }
    let existing = db.kb_metadata_map().unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut changed = 0usize;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() || !is_markdown(entry.path()) {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        seen.insert(rel.clone());
        let mtime = mtime_of(entry.path());
        if existing.get(&rel) == Some(&mtime) {
            continue; // unchanged
        }
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let p = parse_document(&rel, &content);
        if db
            .kb_upsert(&rel, &p.title, &p.body, &p.tags, mtime, p.word_count)
            .is_ok()
        {
            changed += 1;
        }
    }
    for old in existing.keys() {
        if !seen.contains(old) {
            let _ = db.kb_delete(old);
        }
    }
    Ok(changed)
}

/// (Re)index a single file by absolute path — used by the watcher. If the file
/// is gone, drop it from the index.
fn index_file(db: &Db, vault: &str, abs: &Path) -> AppResult<()> {
    let rel = match abs.strip_prefix(Path::new(vault)) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => return Ok(()),
    };
    if has_hidden_component(&rel) {
        return Ok(());
    }
    if abs.is_file() {
        let content = std::fs::read_to_string(abs).unwrap_or_default();
        let p = parse_document(&rel, &content);
        db.kb_upsert(&rel, &p.title, &p.body, &p.tags, mtime_of(abs), p.word_count)?;
    } else {
        db.kb_delete(&rel)?;
    }
    Ok(())
}

/// Write/update a note from agent-supplied content: upsert the index and (when
/// a vault is configured) write the markdown to disk atomically. Returns the
/// normalized vault-relative path.
pub fn ingest_content(
    db: &Db,
    vault: Option<&str>,
    rel_path: &str,
    content: &str,
) -> AppResult<String> {
    let rel = sanitize_rel_path(rel_path)?;
    let p = parse_document(&rel, content);
    db.kb_upsert(&rel, &p.title, &p.body, &p.tags, now(), p.word_count)?;
    if let Some(vault) = vault {
        let abs = Path::new(vault).join(&rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic: write a hidden temp sibling, then rename over the target.
        let fname = abs.file_name().and_then(|n| n.to_str()).unwrap_or("note.md");
        let tmp = abs.with_file_name(format!(".{fname}.flocktmp"));
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &abs)?;
    }
    Ok(rel)
}

/// Delete a note from the index and (when configured) the vault.
pub fn delete_doc(db: &Db, vault: Option<&str>, rel_path: &str) -> AppResult<()> {
    let rel = sanitize_rel_path(rel_path)?;
    db.kb_delete(&rel)?;
    if let Some(vault) = vault {
        let _ = std::fs::remove_file(Path::new(vault).join(&rel));
    }
    Ok(())
}

// ---------- file watcher ----------

/// Boot the indexer: kick off a background scan and start watching the vault.
/// No-op when no vault is configured. Called on app start and after the vault
/// path changes.
pub fn start_indexing(app: AppHandle) {
    let Some(vault) = vault_path() else {
        return;
    };
    {
        let app = app.clone();
        let vault = vault.clone();
        std::thread::spawn(move || {
            let st = app.state::<AppState>();
            if let Err(e) = reindex(&st.db, &vault) {
                eprintln!("flock kb: initial reindex failed: {e}");
            }
        });
    }
    restart_watcher(&app, Some(vault));
}

/// Replace the live vault watcher (dropping the old one stops its thread).
pub fn restart_watcher(app: &AppHandle, vault: Option<String>) {
    let st = app.state::<AppState>();
    *st.kb_watcher.lock().unwrap() = None;
    if let Some(vault) = vault {
        if !vault.trim().is_empty() {
            *st.kb_watcher.lock().unwrap() = build_watcher(app.clone(), vault);
        }
    }
}

fn build_watcher(app: AppHandle, vault: String) -> Option<RecommendedWatcher> {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .ok()?;
    if watcher
        .watch(Path::new(&vault), RecursiveMode::Recursive)
        .is_err()
    {
        return None;
    }
    std::thread::spawn(move || watch_loop(app, vault, rx));
    Some(watcher)
}

/// Debounce raw fs events (~500ms quiet window) and re-index each touched `.md`
/// file. Exits when the channel disconnects (the watcher was replaced/dropped).
fn watch_loop(app: AppHandle, vault: String, rx: Receiver<notify::Result<Event>>) {
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        let first = match rx.recv() {
            Ok(r) => r,
            Err(_) => break,
        };
        let mut pending: HashSet<PathBuf> = HashSet::new();
        collect(first, &mut pending);
        let mut disconnected = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(r) => collect(r, &mut pending),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        let st = app.state::<AppState>();
        for path in &pending {
            if is_markdown(path) {
                let _ = index_file(&st.db, &vault, path);
            }
        }
        if disconnected {
            break;
        }
    }
}

fn collect(res: notify::Result<Event>, set: &mut HashSet<PathBuf>) {
    if let Ok(ev) = res {
        for p in ev.paths {
            set.insert(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_title_and_tags() {
        let md = "---\ntitle: My Note\ntags: [alpha, beta]\n---\n\nbody text here";
        let p = parse_document("notes/x.md", md);
        assert_eq!(p.title, "My Note");
        assert_eq!(p.tags, "alpha beta");
        assert_eq!(p.body.trim(), "body text here");
    }

    #[test]
    fn title_falls_back_to_h1_then_filename() {
        let p = parse_document("notes/deploy.md", "# Deploy process\n\nsteps");
        assert_eq!(p.title, "Deploy process");
        let p = parse_document("notes/no-heading.md", "just text");
        assert_eq!(p.title, "no-heading");
    }

    #[test]
    fn sanitize_query_quotes_tokens_and_drops_specials() {
        assert_eq!(sanitize_query("deploy: render*"), "\"deploy\" \"render\"");
        assert_eq!(sanitize_query("decisión"), "\"decisión\"");
        assert_eq!(sanitize_query("  -- "), "");
    }

    #[test]
    fn sanitize_rel_path_rejects_traversal() {
        assert!(sanitize_rel_path("../escape.md").is_err());
        assert!(sanitize_rel_path("/abs/path.md").is_ok()); // leading slash stripped
        assert_eq!(sanitize_rel_path("memory/x").unwrap(), "memory/x.md");
        assert_eq!(sanitize_rel_path("memory/x.md").unwrap(), "memory/x.md");
    }

    /// End-to-end against a real (temp) DB + vault: walk/skip-hidden/parse,
    /// search, ingest write-back, and prune-on-reindex.
    #[test]
    fn reindex_ingest_and_prune_roundtrip() {
        use crate::db::Db;
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("flock-kb-test-{uniq}"));
        let vault = base.join("vault");
        std::fs::create_dir_all(vault.join("memory")).unwrap();
        std::fs::write(
            vault.join("memory/deploy.md"),
            "---\ntitle: Deploy process\ntags: [ops, render]\n---\n\nWe deploy via Render each morning.",
        )
        .unwrap();
        std::fs::write(vault.join("readme.md"), "# Readme\n\njust notes about pizza").unwrap();
        // A hidden dir (e.g. .obsidian) must be skipped entirely.
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        std::fs::write(vault.join(".obsidian/x.md"), "# secret\nskip me").unwrap();

        let db = Db::open_at(&base.join("t.db")).unwrap();
        let vault_s = vault.to_string_lossy().to_string();

        let n = reindex(&db, &vault_s).unwrap();
        assert_eq!(n, 2, "two md files indexed, .obsidian skipped");

        let hits = db.kb_search(&sanitize_query("deploy render"), 10).unwrap();
        assert!(hits.iter().any(|h| h.path == "memory/deploy.md"));

        let doc = db.kb_get("memory/deploy.md").unwrap();
        assert_eq!(doc.title, "Deploy process");
        assert!(doc.tags.contains(&"render".to_string()));

        // Ingest writes the file to the vault on disk AND indexes it.
        let rel = ingest_content(
            &db,
            Some(&vault_s),
            "memory/learned",
            "---\ntitle: Learned\n---\n\nthe user prefers iterm aesthetics",
        )
        .unwrap();
        assert_eq!(rel, "memory/learned.md");
        assert!(vault.join("memory/learned.md").is_file());
        let hits = db.kb_search(&sanitize_query("iterm"), 10).unwrap();
        assert!(hits.iter().any(|h| h.path == "memory/learned.md"));

        // Deleting a file then re-scanning prunes it from the index.
        std::fs::remove_file(vault.join("readme.md")).unwrap();
        reindex(&db, &vault_s).unwrap();
        assert!(db.kb_get("readme.md").is_err());

        let _ = std::fs::remove_dir_all(&base);
    }
}
