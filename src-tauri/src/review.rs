//! On-demand PR explanations. No model calls from loading, polling, or cache reads.
use crate::{
    db::Worktree,
    env_profiles,
    error::{AppError, AppResult},
    git,
    state::AppState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Target {
    pub repo: String,
    pub own: bool,
    pub worktree_id: i64,
    pub pr_url: Option<String>,
    pub base_sha: String,
    pub head_sha: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowStep {
    pub label_es: String,
    pub changed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Flow {
    pub who_es: String,
    pub steps: Vec<FlowStep>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub what_es: String,
    pub flows: Vec<Flow>,
    pub verdict_es: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    pub name_es: String,
    pub code_name: String,
    pub what_es: String,
    pub side: String,
    pub start: u32,
    pub end: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Eval {
    pub id: String,
    pub severity: String,
    pub kind: String,
    pub side: String,
    pub start: u32,
    pub end: u32,
    pub text_es: String,
    pub fix_es: String,
    pub comment_en: String,
    pub comment_es: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewFile {
    pub path: String,
    pub status: String,
    pub diff_hash: String,
    pub skipped: bool,
    pub skip_reason_es: Option<String>,
    pub name_es: String,
    pub role_es: String,
    pub parts: Vec<Part>,
    pub evals: Vec<Eval>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
}
impl Usage {
    fn add(&mut self, other: &Self) {
        self.model = other.model.clone();
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cost_usd += other.cost_usd;
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    pub version: u8,
    pub kind: String,
    pub target: Target,
    pub summary: Summary,
    pub files: Vec<ReviewFile>,
    pub usage: Usage,
    pub generated_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: String,
    pub text: String,
    pub left: Option<u32>,
    pub right: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub status: String,
    pub diff_hash: String,
    pub diff: String,
    pub skip_reason_es: Option<String>,
    pub lines: Vec<DiffLine>,
}
#[derive(Serialize, Deserialize)]
pub struct ReviewView {
    pub worktree_id: i64,
    pub head_sha: String,
    pub review: Option<Review>,
    pub files: Vec<DiffFile>,
    pub cached: bool,
    pub last_cost_usd: f64,
    pub error: Option<String>,
    #[serde(default)]
    pub deep_available: bool,
    #[serde(default)]
    pub deep_error: Option<String>,
}

pub fn data_dir() -> AppResult<PathBuf> {
    Ok(dirs::data_local_dir()
        .ok_or_else(|| AppError::msg("No se encontró la carpeta de Flock"))?
        .join("Flock")
        .join("reviews"))
}
pub fn valid_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn cache_path(root: &Path, id: i64, sha: &str, deep: bool) -> AppResult<PathBuf> {
    if id <= 0 || !valid_sha(sha) {
        return Err(AppError::msg("Identificador de revisión inválido"));
    }
    Ok(root
        .join(id.to_string())
        .join(format!("{sha}{}.json", if deep { ".deep" } else { "" })))
}
pub fn write_json(path: &Path, value: &impl Serialize) -> AppResult<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(value).map_err(|e| AppError::msg(e.to_string()))?,
    )?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> AppResult<T> {
    serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|e| AppError::msg(format!("No se pudo leer la revisión: {e}")))
}

/// GUI launches omit Homebrew and ~/.local/bin. Never execute a bare binary name.
pub fn binary(name: &str) -> AppResult<PathBuf> {
    let mut candidates = vec![
        PathBuf::from(format!("/opt/homebrew/bin/{name}")),
        PathBuf::from(format!("/usr/local/bin/{name}")),
        PathBuf::from(format!("/usr/bin/{name}")),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.insert(0, home.join(".local/bin").join(name));
    }
    if let Some(p) = candidates.into_iter().find(|p| p.is_file()) {
        return Ok(p);
    }
    // `name` is only ever a literal controlled by this module.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = Command::new(shell)
        .args(["-ilc", &format!("command -v {name}")])
        .output()?;
    for s in String::from_utf8_lossy(&out.stdout).lines().rev() {
        let p = PathBuf::from(s.trim());
        if p.is_absolute() && p.is_file() {
            return Ok(p);
        }
    }
    Err(AppError::msg(format!("No se encontró {name}")))
}
pub fn run(cwd: &Path, name: &str, args: &[&str], env: &[(String, String)]) -> AppResult<String> {
    let out = Command::new(binary(name)?)
        .current_dir(cwd)
        .envs(env.iter().cloned())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(AppError::msg(format!(
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
pub fn worktree_env(state: &AppState, w: &Worktree) -> AppResult<Vec<(String, String)>> {
    let repo = state.db.get_repo(w.repo_id)?;
    Ok(env_profiles::resolve_vars_for_worktree(
        &env_profiles::load(),
        w.env_profile.as_deref(),
        &repo.path,
    ))
}
fn head(cwd: &Path) -> AppResult<String> {
    Ok(run(cwd, "git", &["rev-parse", "HEAD"], &[])?.trim().into())
}
pub fn diff_args(base: &str, head: &str) -> Vec<String> {
    vec![
        "-c".into(),
        "core.quotePath=false".into(),
        "diff".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--no-color".into(),
        "--find-renames".into(),
        format!("{base}..{head}"),
        "--".into(),
    ]
}
pub fn diff_between(cwd: &Path, base: &str, head: &str) -> AppResult<Vec<DiffFile>> {
    if !valid_sha(base) || !valid_sha(head) {
        return Err(AppError::msg("Commits inválidos"));
    }
    let args = diff_args(base, head);
    Ok(parse_diff(&run(
        cwd,
        "git",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
        &[],
    )?))
}
pub fn merge_base(cwd: &Path, env: &[(String, String)]) -> AppResult<String> {
    run(cwd, "git", &["fetch", "origin"], env)?;
    let default = git::detect_default_branch(cwd)?;
    Ok(run(
        cwd,
        "git",
        &["merge-base", &format!("origin/{default}"), "HEAD"],
        env,
    )?
    .trim()
    .into())
}

fn git_path(s: &str) -> String {
    let s = s.split('\t').next().unwrap_or(s);
    if !s.starts_with('"') {
        return s.to_string();
    }
    let bytes = s.trim_matches('"').as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            if (b'0'..=b'7').contains(&bytes[i]) {
                let mut n: u16 = 0;
                for _ in 0..3 {
                    if i < bytes.len() && (b'0'..=b'7').contains(&bytes[i]) {
                        n = n * 8 + (bytes[i] - b'0') as u16;
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(n as u8);
                continue;
            }
            out.push(match bytes[i] {
                b't' => b'\t',
                b'n' => b'\n',
                b'r' => b'\r',
                c => c,
            });
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
pub fn parse_diff(text: &str) -> Vec<DiffFile> {
    let mut chunks = Vec::<String>::new();
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            chunks.push(String::new());
        }
        if let Some(c) = chunks.last_mut() {
            c.push_str(line);
            c.push('\n');
        }
    }
    chunks
        .into_iter()
        .map(|diff| {
            let mut path = String::new();
            let mut status = "modified";
            let mut lines = Vec::new();
            let (mut left, mut right) = (0, 0);
            let mut hunk = false;
            for l in diff.lines() {
                if let Some(s) = l.strip_prefix("--- ") {
                    if s != "/dev/null" {
                        path = {
                            let p = git_path(s);
                            p.strip_prefix("a/").unwrap_or(&p).into()
                        };
                    }
                }
                if let Some(s) = l.strip_prefix("+++ ") {
                    if s != "/dev/null" {
                        path = {
                            let p = git_path(s);
                            p.strip_prefix("b/").unwrap_or(&p).into()
                        };
                    }
                }
                if l.starts_with("new file mode ") {
                    status = "added";
                }
                if l.starts_with("deleted file mode ") {
                    status = "deleted";
                }
                if let Some(s) = l.strip_prefix("rename to ") {
                    status = "renamed";
                    path = git_path(s);
                }
                if l.starts_with("@@ ") {
                    let fields: Vec<_> = l.split_whitespace().collect();
                    left = fields
                        .get(1)
                        .and_then(|s| s.trim_start_matches('-').split(',').next()?.parse().ok())
                        .unwrap_or(0);
                    right = fields
                        .get(2)
                        .and_then(|s| s.trim_start_matches('+').split(',').next()?.parse().ok())
                        .unwrap_or(0);
                    lines.push(DiffLine {
                        kind: "hunk".into(),
                        text: l.into(),
                        left: None,
                        right: None,
                    });
                    hunk = true;
                    continue;
                }
                if hunk {
                    let (kind, old, new) = match l.as_bytes().first() {
                        Some(b'+') => {
                            let n = right;
                            right += 1;
                            ("add", None, Some(n))
                        }
                        Some(b'-') => {
                            let n = left;
                            left += 1;
                            ("delete", Some(n), None)
                        }
                        Some(b' ') => {
                            let (a, b) = (left, right);
                            left += 1;
                            right += 1;
                            ("context", Some(a), Some(b))
                        }
                        _ => continue,
                    };
                    lines.push(DiffLine {
                        kind: kind.into(),
                        text: l[1..].into(),
                        left: old,
                        right: new,
                    });
                }
            }
            if path.is_empty() {
                // Binary/mode-only changes have no --- / +++ headers.
                if let Some(s) = diff
                    .lines()
                    .next()
                    .and_then(|s| s.rsplit_once(" b/").map(|(_, s)| s))
                {
                    path = s.trim_end_matches('"').into();
                }
            }
            let diff_hash = format!("{:x}", Sha1::digest(diff.as_bytes()));
            let skip_reason_es = skip_reason(&path, &diff, &lines);
            DiffFile {
                path,
                status: status.into(),
                diff_hash,
                diff,
                skip_reason_es,
                lines,
            }
        })
        .collect()
}
pub fn skip_reason(path: &str, diff: &str, lines: &[DiffLine]) -> Option<String> {
    let p = path.to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    let reason = if name.ends_with(".lock")
        || name.ends_with("-lock.json")
        || name == "yarn.lock"
        || name == "pnpm-lock.yaml"
        || name == "gemfile.lock"
    {
        "Archivo de dependencias fijadas"
    } else if p.contains("/__snapshots__/") || p.ends_with(".snap") || p.ends_with(".snap.new") {
        "Snapshot generado"
    } else if p.split('/').any(|s| {
        [
            "vendor",
            "vendored",
            "node_modules",
            "dist",
            "build",
            "generated",
            "__generated__",
        ]
        .contains(&s)
    }) || p.ends_with(".generated.ts")
        || p.ends_with(".g.rs")
        || p.ends_with(".pb.go")
        || p.ends_with(".map")
        || diff.lines().take(20).any(|s| {
            let s = s.to_lowercase();
            s.contains("@generated")
                || s.contains("code generated")
                || s.contains("auto-generated")
                || s.contains("autogenerated")
        })
    {
        "Archivo generado o de terceros"
    } else if p.ends_with(".min.js")
        || p.ends_with(".min.css")
        || lines.iter().any(|l| l.text.len() > 2000)
    {
        "Archivo minificado"
    } else if diff.contains("Binary files ") || diff.contains("GIT binary patch") {
        "Archivo binario"
    } else if lines
        .iter()
        .filter(|l| l.kind == "add" || l.kind == "delete")
        .count()
        > 800
    {
        "Más de 800 líneas cambiadas"
    } else if lines.is_empty() {
        "Solo cambia el nombre o los permisos"
    } else {
        return None;
    };
    Some(reason.into())
}
pub fn changed_files<'a>(files: &'a [DiffFile], previous: Option<&Review>) -> Vec<&'a DiffFile> {
    files
        .iter()
        .filter(|f| {
            f.skip_reason_es.is_none()
                && !previous.is_some_and(|r| {
                    r.files.iter().any(|old| {
                        old.path == f.path && old.diff_hash == f.diff_hash && !old.skipped
                    })
                })
        })
        .collect()
}
pub fn check_range(file: &DiffFile, side: &str, start: u32, end: u32) -> AppResult<()> {
    if !["LEFT", "RIGHT"].contains(&side) || start == 0 || end < start {
        return Err(AppError::msg(format!(
            "{}: ubicación inválida {side}:{start}-{end}",
            file.path
        )));
    }
    let has = |n| {
        file.lines.iter().any(|l| {
            if side == "RIGHT" {
                l.right == Some(n)
            } else {
                l.left == Some(n)
            }
        })
    };
    if !has(start) || !has(end) {
        return Err(AppError::msg(format!(
            "{}: la línea {side}:{start}-{end} no está en el diff",
            file.path
        )));
    }
    Ok(())
}
pub fn validate(review: &Review, target: &Target, files: &[DiffFile], kind: &str) -> AppResult<()> {
    let fail = |s: &str| AppError::msg(format!("Formato de revisión inválido: {s}"));
    if review.version != 1 || review.kind != kind || review.target != *target {
        return Err(fail("versión, tipo o commits distintos"));
    }
    if review.summary.what_es.trim().is_empty()
        || review.summary.verdict_es.trim().is_empty()
        || chrono::DateTime::parse_from_rfc3339(&review.generated_at).is_err()
    {
        return Err(fail("resumen o fecha incompletos"));
    }
    if !review.usage.cost_usd.is_finite() || review.usage.cost_usd < 0. {
        return Err(fail("costo inválido"));
    }
    let mut paths = HashSet::new();
    let mut ids = HashSet::new();
    if review.files.len() != files.len() {
        return Err(fail("faltan archivos"));
    }
    for f in &review.files {
        let d = files
            .iter()
            .find(|d| d.path == f.path)
            .ok_or_else(|| fail("archivo desconocido"))?;
        if !paths.insert(&f.path) || f.diff_hash != d.diff_hash || f.status != d.status {
            return Err(fail("archivo repetido, estado o hash incorrecto"));
        }
        if f.skipped {
            if d.skip_reason_es.is_none()
                || f.skip_reason_es.as_deref().unwrap_or("").is_empty()
                || !f.evals.is_empty()
                || !f.parts.is_empty()
            {
                return Err(fail("archivo omitido con evaluación"));
            }
            continue;
        }
        if d.skip_reason_es.is_some()
            || f.skip_reason_es.is_some()
            || f.name_es.trim().is_empty()
            || f.role_es.trim().is_empty()
            || f.parts.is_empty()
            || f.evals.is_empty()
        {
            return Err(fail("archivo sin explicación o evaluación"));
        }
        for p in &f.parts {
            check_range(d, &p.side, p.start, p.end)?;
            if p.name_es.trim().is_empty() || p.what_es.trim().is_empty() {
                return Err(fail("parte sin nombre o explicación"));
            }
        }
        let all_ok = f.evals.iter().all(|e| e.severity == "ok");
        if all_ok && (f.evals.len() != 1 || f.evals[0].kind != "ok") {
            return Err(fail(
                "un archivo sin problemas lleva una sola evaluación ok",
            ));
        }
        for e in &f.evals {
            if e.id.is_empty()
                || !ids.insert(&e.id)
                || !["warn", "info", "ok"].contains(&e.severity.as_str())
                || !["comment", "naming", "logic", "test", "scope", "bug", "ok"]
                    .contains(&e.kind.as_str())
                || (e.kind == "ok") != (e.severity == "ok")
                || e.text_es.trim().is_empty()
            {
                return Err(fail("evaluación inválida"));
            }
            check_range(d, &e.side, e.start, e.end)?;
            if e.severity != "ok"
                && [
                    e.fix_es.as_str(),
                    e.comment_en.as_str(),
                    e.comment_es.as_str(),
                ]
                .iter()
                .any(|s| s.trim().is_empty())
            {
                return Err(fail("falta el cambio sugerido o su traducción"));
            }
        }
    }
    Ok(())
}

pub fn load(state: &AppState, id: i64) -> AppResult<ReviewView> {
    load_at(state, id, &data_dir()?, false)
}
pub fn load_selected(state: &AppState, id: i64, deep: Option<bool>) -> AppResult<ReviewView> {
    let root = data_dir()?;
    let quick = load_at(state, id, &root, false)?;
    let selected = root.join(id.to_string()).join("selected-kind");
    if let Some(deep) = deep {
        std::fs::create_dir_all(selected.parent().unwrap())?;
        std::fs::write(&selected, if deep { "deep" } else { "quick" })?;
    }
    let use_deep = deep.unwrap_or_else(|| {
        std::fs::read_to_string(&selected)
            .map(|s| s == "deep")
            .unwrap_or(quick.deep_available)
    });
    if use_deep && quick.deep_available {
        load_at(state, id, &root, true)
    } else {
        Ok(quick)
    }
}
fn load_at(state: &AppState, id: i64, root: &Path, deep: bool) -> AppResult<ReviewView> {
    let w = state.db.get_worktree(id)?;
    if w.kind == "orchestrator" {
        return Err(AppError::msg("Elegí un worktree con código"));
    }
    let cwd = Path::new(&w.path);
    let sha = if w.kind == "review" {
        crate::review_github::source_head(&w)?
    } else {
        head(cwd)?
    };
    let current = cache_path(&root, id, &sha, deep)?;
    let path = if current.exists() {
        Some(current)
    } else if deep {
        None
    } else {
        std::fs::read_to_string(root.join(id.to_string()).join("latest"))
            .ok()
            .and_then(|s| cache_path(&root, id, s.trim(), false).ok())
            .filter(|p| p.exists())
    };
    let mut view = ReviewView {
        worktree_id: id,
        head_sha: sha.clone(),
        review: None,
        files: vec![],
        cached: false,
        last_cost_usd: 0.,
        error: None,
        deep_available: false,
        deep_error: None,
    };
    let dir = root.join(id.to_string());
    let deep_path = cache_path(root, id, &sha, true)?;
    if deep_path.exists() {
        let validation = read_json::<Review>(&deep_path).and_then(|r| {
            let files = if w.kind == "review" {
                crate::review_github::files(&w, &r.target.base_sha, &r.target.head_sha)?
            } else {
                diff_between(cwd, &r.target.base_sha, &r.target.head_sha)?
            };
            if r.target.worktree_id != id
                || r.target.head_sha != sha
                || r.target.own != (w.kind != "review")
            {
                return Err(AppError::msg(
                    "La revisión profunda pertenece a otro commit o worktree",
                ));
            }
            let expected = read_json::<Target>(&deep_path.with_extension("target.json"))
                .unwrap_or_else(|_| r.target.clone());
            validate(&r, &expected, &files, "deep")
        });
        match validation {
            Ok(()) => view.deep_available = true,
            Err(e) => view.deep_error = Some(e.to_string()),
        }
    }
    if let Ok(usage) = read_json::<Usage>(&dir.join(format!("{sha}.run.json"))) {
        view.last_cost_usd = usage.cost_usd;
    }
    view.error = std::fs::read_to_string(dir.join(format!("{sha}.error"))).ok();
    if let Some(path) = path {
        let r: Review = read_json(&path)?;
        if r.target.worktree_id != id || r.target.own != (w.kind != "review") {
            return Err(AppError::msg("La revisión pertenece a otro worktree"));
        }
        view.files = if w.kind == "review" {
            crate::review_github::files(&w, &r.target.base_sha, &r.target.head_sha)?
        } else {
            diff_between(cwd, &r.target.base_sha, &r.target.head_sha)?
        };
        validate(
            &r,
            &r.target,
            &view.files,
            if deep { "deep" } else { "quick" },
        )?;
        if view.last_cost_usd == 0. {
            view.last_cost_usd =
                read_json::<Usage>(&dir.join(format!("{}.run.json", r.target.head_sha)))
                    .map(|u| u.cost_usd)
                    .unwrap_or(0.);
        }
        view.cached = true;
        view.review = Some(r);
    }
    Ok(view)
}
fn generation_lock(id: i64) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<i64, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(id)
        .or_default()
        .clone()
}

/// Single fresh, bounded, read-only Claude invocation. Kept separate from worktree PTYs.
pub fn sonnet(
    cwd: &Path,
    env: &[(String, String)],
    prompt: &str,
    system: &str,
) -> AppResult<(String, Usage)> {
    let mut child = Command::new(binary("claude")?)
        .current_dir(cwd)
        .envs(env.iter().cloned())
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .args([
            "-p",
            "--model",
            "sonnet",
            "--effort",
            "low",
            "--output-format",
            "json",
            "--tools",
            "Read,Grep,Glob",
            "--allowedTools",
            "Read,Grep,Glob",
            "--permission-mode",
            "dontAsk",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--safe-mode",
            "--no-session-persistence",
            "--max-turns",
            "5",
            "--max-budget-usd",
            "1.50",
            "--system-prompt",
            system,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let input = prompt.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).map(|_| s)
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).map(|_| s)
    });
    let deadline = Instant::now() + Duration::from_secs(240);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::msg(
                "La generación tardó más de cuatro minutos. Podés volver a intentarlo.",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = writer.join();
    let raw = out
        .join()
        .map_err(|_| AppError::msg("No se pudo leer la respuesta"))??;
    let stderr = err.join().ok().and_then(Result::ok).unwrap_or_default();
    let v: Value = serde_json::from_str(&raw).map_err(|_| {
        AppError::msg(format!(
            "Claude no devolvió JSON: {}",
            if stderr.is_empty() {
                raw.chars().take(600).collect::<String>()
            } else {
                stderr.chars().take(600).collect()
            }
        ))
    })?;
    let u = &v["usage"];
    let usage = Usage {
        model: "sonnet".into(),
        input_tokens: u["input_tokens"].as_u64().unwrap_or(0)
            + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.),
    };
    let result = v["result"].as_str().unwrap_or("");
    if !status.success() || v["is_error"].as_bool() == Some(true) {
        return Ok((format!("Error de Claude: {result} {}", v["subtype"]), usage));
    }
    Ok((result.to_string(), usage))
}
fn parse_reply(raw: &str) -> AppResult<Review> {
    let s = raw
        .trim()
        .strip_prefix("```json")
        .or_else(|| raw.trim().strip_prefix("```"))
        .unwrap_or(raw.trim())
        .trim();
    let s = s.strip_suffix("```").unwrap_or(s).trim();
    serde_json::from_str(s).map_err(|e| AppError::msg(format!("JSON inválido: {e}")))
}
pub fn generate(state: &AppState, id: i64) -> AppResult<ReviewView> {
    generate_at(state, id, &data_dir()?)
}
fn generate_at(state: &AppState, id: i64, root: &Path) -> AppResult<ReviewView> {
    let lock = generation_lock(id);
    let _guard = lock.lock().unwrap();
    let w = state.db.get_worktree(id)?;
    if w.kind == "review" {
        crate::review_github::refresh(state, &w)?;
    }
    let view = load_at(state, id, root, false)?;
    if view
        .review
        .as_ref()
        .is_some_and(|r| r.target.head_sha == view.head_sha)
    {
        return Ok(view);
    }
    let w = state.db.get_worktree(id)?;
    let repo = state.db.get_repo(w.repo_id)?;
    let cwd = Path::new(&w.path);
    let env = worktree_env(state, &w)?;
    let (target, files, pr) = if w.kind == "review" {
        let source = crate::review_github::source(id)?;
        let target = crate::review_github::target(&w)?;
        let files = crate::review_github::files(&w, &target.base_sha, &target.head_sha)?;
        (
            target,
            files,
            json!({"title":source.title,"body":source.body,"url":source.pr.url}),
        )
    } else {
        let base = merge_base(cwd, &env)?;
        let sha = head(cwd)?;
        let files = diff_between(cwd, &base, &sha)?;
        let pr: Value = run(cwd, "gh", &["pr", "view", "--json", "title,body,url"], &env)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        (
            Target {
                repo: repo.name,
                own: true,
                worktree_id: id,
                pr_url: pr["url"].as_str().map(String::from),
                base_sha: base,
                head_sha: sha,
            },
            files,
            pr,
        )
    };
    let sha = target.head_sha.clone();
    if files.is_empty() && view.review.is_none() {
        return Err(AppError::msg(
            "No hay cambios para explicar contra la base de la rama",
        ));
    }
    let changed = changed_files(&files, view.review.as_ref());
    // Compact prior context; do not resend unchanged diffs or their assessments.
    let previous = view.review.as_ref().map(|r| json!({"summary":r.summary,"files":r.files.iter().map(|f|json!({"path":f.path,"name_es":f.name_es,"role_es":f.role_es,"skipped":f.skipped,"evals":f.evals.iter().filter(|e|e.severity!="ok").map(|e|json!({"severity":e.severity,"kind":e.kind,"text_es":e.text_es})).collect::<Vec<_>>()})).collect::<Vec<_>>()}));
    let manifest: Vec<_> = files.iter().map(|f|json!({"path":f.path,"status":f.status,"diff_hash":f.diff_hash,"skip_reason_es":f.skip_reason_es})).collect();
    let input = json!({"target":target,"branch":w.branch,"pr":pr,"previous":previous,"manifest":manifest,
        "changed_files": changed.iter().map(|f|json!({"path":f.path,"status":f.status,"diff_hash":f.diff_hash,"diff":f.diff})).collect::<Vec<_>>(),
        "removed_files":view.review.as_ref().map(|r|r.files.iter().filter(|f|!files.iter().any(|d|d.path==f.path)).map(|f|&f.path).collect::<Vec<_>>())});
    let path = cache_path(root, id, &sha, false)?;
    let dir = path.parent().unwrap();
    let run_path = dir.join(format!("{sha}.run.json"));
    let mut spent = read_json::<Usage>(&run_path).unwrap_or_default();
    let mut prompt = input.to_string();
    let mut last_error = String::new();
    for attempt in 0..2 {
        let (raw, usage) = sonnet(
            cwd,
            &env,
            &prompt,
            include_str!("../prompts/review-quick.md"),
        )?;
        spent.add(&usage);
        write_json(&run_path, &spent)?;
        let parsed = parse_reply(&raw).and_then(|mut r| {
            // The model returns only changed, explained files. Flock supplies skipped and unchanged entries.
            let expected: HashSet<_> = changed.iter().map(|f| f.path.as_str()).collect();
            let actual: HashSet<_> = r.files.iter().map(|f| f.path.as_str()).collect();
            if actual != expected || r.files.len() != expected.len() {
                return Err(AppError::msg(
                    "Devolvé exactamente los archivos de changed_files",
                ));
            }
            for d in &files {
                if let Some(reason) = &d.skip_reason_es {
                    r.files.push(ReviewFile {
                        path: d.path.clone(),
                        status: d.status.clone(),
                        diff_hash: d.diff_hash.clone(),
                        skipped: true,
                        skip_reason_es: Some(reason.clone()),
                        name_es: d.path.clone(),
                        role_es: String::new(),
                        parts: vec![],
                        evals: vec![],
                    });
                } else if !expected.contains(d.path.as_str()) {
                    if let Some(f) = view
                        .review
                        .as_ref()
                        .and_then(|r| r.files.iter().find(|f| f.path == d.path))
                    {
                        r.files.push(f.clone());
                    }
                }
            }
            r.files.sort_by_key(|f| {
                files
                    .iter()
                    .position(|d| d.path == f.path)
                    .unwrap_or(usize::MAX)
            });
            r.usage = view
                .review
                .as_ref()
                .map(|r| r.usage.clone())
                .unwrap_or_default();
            r.usage.add(&spent);
            validate(&r, &target, &files, "quick")?;
            Ok(r)
        });
        match parsed {
            Ok(r) => {
                write_json(&path, &r)?;
                std::fs::write(dir.join("latest"), &sha)?;
                let _ = std::fs::remove_file(dir.join(format!("{sha}.error")));
                return load_at(state, id, root, false);
            }
            Err(e) => {
                last_error = e.to_string();
                if attempt == 0 {
                    prompt = format!("{input}\n\nLa respuesta anterior no cumple el contrato: {last_error}. Corregila. Respuesta anterior:\n{raw}");
                }
            }
        }
    }
    std::fs::write(dir.join(format!("{sha}.error")), &last_error)?;
    Err(AppError::msg(format!(
        "No se pudo generar la revisión: {last_error}. Costo de los intentos: US${:.4}",
        spent.cost_usd
    )))
}
#[derive(Clone, Serialize)]
pub struct Opened {
    pub worktree_id: i64,
    pub status: String,
    pub eval_count: usize,
    pub cost_usd: f64,
}
pub fn open_request(
    app: &AppHandle,
    worktree_id: Option<i64>,
    pr_url: Option<String>,
    should_generate: bool,
    parent_id: Option<i64>,
) -> AppResult<Opened> {
    let id = match (worktree_id, pr_url) {
        (None, Some(url)) => crate::review_github::open_pr(app, &url, parent_id)?.id,
        (Some(id), None) => {
            let state = app.state::<AppState>();
            let w = state.db.get_worktree(id)?;
            if w.kind == "review" {
                crate::review_github::refresh(&state, &w)?;
            }
            id
        }
        _ => return Err(AppError::msg("Indicá worktree_id o pr_url")),
    };
    open(app, id, should_generate)
}
pub fn open(app: &AppHandle, id: i64, should_generate: bool) -> AppResult<Opened> {
    let state = app.state::<AppState>();
    let view = load(&state, id)?;
    let _ = app.emit("review:open", id);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
    let current = view
        .review
        .as_ref()
        .is_some_and(|r| r.target.head_sha == view.head_sha);
    let view = if !current && should_generate {
        let result = generate(&state, id);
        let _ = app.emit("review:changed", id);
        result?
    } else {
        view
    };
    let displayed = load_selected(&state, id, None)?;
    Ok(Opened {
        worktree_id: id,
        eval_count: displayed
            .review
            .as_ref()
            .map(|r| crate::review_access::evals(r).len())
            .unwrap_or(0),
        status: if view
            .review
            .as_ref()
            .is_some_and(|r| r.target.head_sha == view.head_sha)
        {
            if current {
                "cached"
            } else {
                "generated"
            }
        } else {
            "pending"
        }
        .into(),
        cost_usd: view
            .review
            .map(|r| r.usage.cost_usd)
            .unwrap_or(view.last_cost_usd),
    })
}
/// Only observes git/cache files. It never generates, even when commits change.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = HashMap::new();
        let mut refreshed = HashMap::<i64, Instant>::new();
        loop {
            let state = app.state::<AppState>();
            if let Ok(repos) = state.db.list_repos() {
                for repo in repos {
                    if let Ok(worktrees) = state.db.list_worktrees(repo.id) {
                        for w in worktrees {
                            if w.kind == "orchestrator"
                                || !data_dir()
                                    .map(|d| d.join(w.id.to_string()).exists())
                                    .unwrap_or(false)
                            {
                                continue;
                            }
                            if w.kind == "review"
                                && refreshed
                                    .get(&w.id)
                                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
                            {
                                let _ = crate::review_github::refresh(&state, &w);
                                refreshed.insert(w.id, Instant::now());
                            }
                            let current = if w.kind == "review" {
                                crate::review_github::source_head(&w)
                            } else {
                                head(Path::new(&w.path))
                            };
                            if let Ok(sha) = current {
                                let deep_stamp = cache_path(&data_dir().unwrap(), w.id, &sha, true)
                                    .ok()
                                    .and_then(|p| std::fs::metadata(p).ok())
                                    .and_then(|m| m.modified().ok());
                                let signature = (sha, deep_stamp);
                                if last.insert(w.id, signature.clone()).as_ref() != Some(&signature)
                                {
                                    if deep_stamp.is_some()
                                        && load_at(&state, w.id, &data_dir().unwrap(), false)
                                            .is_ok_and(|v| v.deep_available)
                                    {
                                        let _ = std::fs::write(
                                            data_dir()
                                                .unwrap()
                                                .join(w.id.to_string())
                                                .join("selected-kind"),
                                            "deep",
                                        );
                                    }
                                    let _ = app.emit("review:changed", w.id);
                                }
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    const PATCH: &str = "diff --git a/a/file.rb b/a/file.rb\nindex 111..222 100644\n--- a/a/file.rb\n+++ b/a/file.rb\n@@ -10,3 +10,4 @@ method\n context\n-old\n+new\n+extra\n end\n@@ -42,1 +43,1 @@\n-last\n+final\n";
    fn fixture() -> (Target, Vec<DiffFile>, Review) {
        let files = parse_diff(PATCH);
        let target = Target {
            repo: "demo".into(),
            own: true,
            worktree_id: 1,
            pr_url: None,
            base_sha: "1".repeat(40),
            head_sha: "2".repeat(40),
        };
        let review = Review {
            version: 1,
            kind: "quick".into(),
            target: target.clone(),
            summary: Summary {
                what_es: "Cambia una respuesta.".into(),
                flows: vec![],
                verdict_es: "Está bien.".into(),
            },
            files: vec![ReviewFile {
                path: files[0].path.clone(),
                status: "modified".into(),
                diff_hash: files[0].diff_hash.clone(),
                skipped: false,
                skip_reason_es: None,
                name_es: "Respuesta".into(),
                role_es: "Devuelve el resultado.".into(),
                parts: vec![Part {
                    name_es: "Responder".into(),
                    code_name: "method".into(),
                    what_es: "Agrega un dato.".into(),
                    side: "RIGHT".into(),
                    start: 11,
                    end: 12,
                }],
                evals: vec![Eval {
                    id: "f1ok".into(),
                    severity: "ok".into(),
                    kind: "ok".into(),
                    side: "LEFT".into(),
                    start: 11,
                    end: 11,
                    text_es: "Se ve bien.".into(),
                    fix_es: String::new(),
                    comment_en: String::new(),
                    comment_es: String::new(),
                }],
            }],
            usage: Usage::default(),
            generated_at: "2026-10-28T12:00:00Z".into(),
        };
        (target, files, review)
    }
    #[test]
    fn hunks_map_to_real_left_and_right_lines() {
        let files = parse_diff(PATCH);
        let f = &files[0];
        assert_eq!(f.path, "a/file.rb");
        assert_eq!((f.lines[1].left, f.lines[1].right), (Some(10), Some(10)));
        assert_eq!((f.lines[2].left, f.lines[2].right), (Some(11), None));
        assert_eq!((f.lines[3].left, f.lines[3].right), (None, Some(11)));
        assert_eq!((f.lines[4].left, f.lines[4].right), (None, Some(12)));
        assert_eq!((f.lines[7].left, f.lines[7].right), (Some(42), None));
        assert_eq!((f.lines[8].left, f.lines[8].right), (None, Some(43)));
    }
    #[test]
    fn handles_added_deleted_renamed_and_quoted_paths() {
        for (header, expected, side) in [
            (
                "new file mode 100644\n--- /dev/null\n+++ b/new.rb\n@@ -0,0 +1 @@\n+hello\n",
                "added",
                "RIGHT",
            ),
            (
                "deleted file mode 100644\n--- a/new.rb\n+++ /dev/null\n@@ -1 +0,0 @@\n-hello\n",
                "deleted",
                "LEFT",
            ),
        ] {
            let f = parse_diff(&format!("diff --git a/new.rb b/new.rb\n{header}"));
            assert_eq!(f[0].status, expected);
            assert_eq!(f[0].path, "new.rb");
            check_range(&f[0], side, 1, 1).unwrap();
        }
        let f = parse_diff("diff --git a/old b/new name\nsimilarity index 100%\nrename from old\nrename to new name\n");
        assert_eq!(f[0].path, "new name");
        assert_eq!(f[0].status, "renamed");
        assert!(f[0].skip_reason_es.is_some());
        assert_eq!(git_path("\"b/caf\\303\\251.rb\""), "b/café.rb");
    }
    #[test]
    fn detects_skipped_files_without_hiding_them() {
        let lines = parse_diff(PATCH)[0].lines.clone();
        for p in [
            "Cargo.lock",
            "Gemfile.lock",
            "package-lock.json",
            "pnpm-lock.yaml",
            "src/__snapshots__/a.snap",
            "vendor/a.rb",
            "generated/a.ts",
            "src/foo.generated.ts",
            "x.min.js",
        ] {
            assert!(skip_reason(p, PATCH, &lines).is_some(), "{p}");
        }
        assert!(skip_reason("normal.rs", PATCH, &lines).is_none());
        assert!(skip_reason(
            "image.png",
            "Binary files a/image.png and b/image.png differ",
            &[]
        )
        .is_some());
        let mut many = lines;
        let l = many[3].clone();
        many.extend(std::iter::repeat(l).take(801));
        assert!(skip_reason("large.rb", PATCH, &many)
            .unwrap()
            .contains("800"));
    }
    #[test]
    fn diff_command_uses_resolved_merge_base_not_origin_tip() {
        let args = diff_args(&"a".repeat(40), &"b".repeat(40));
        assert!(args.contains(&format!("{}..{}", "a".repeat(40), "b".repeat(40))));
        assert!(!args.iter().any(|s| s.contains("origin/")));
    }
    #[test]
    fn cache_keys_isolate_worktrees_commits_and_depth() {
        let root = Path::new("/tmp/reviews");
        let sha = "a".repeat(40);
        let a = cache_path(root, 1, &sha, false).unwrap();
        assert_ne!(a, cache_path(root, 2, &sha, false).unwrap());
        assert_ne!(a, cache_path(root, 1, &"b".repeat(40), false).unwrap());
        assert_ne!(a, cache_path(root, 1, &sha, true).unwrap());
        assert!(cache_path(root, 1, "../../outside", false).is_err());
    }
    #[test]
    fn incremental_selection_reuses_identical_diffs_and_omits_removed_files() {
        let (_, mut files, review) = fixture();
        assert!(changed_files(&files, Some(&review)).is_empty());
        files[0].diff_hash = "changed".into();
        assert_eq!(changed_files(&files, Some(&review)).len(), 1);
        let mut new = files[0].clone();
        new.path = "new.rb".into();
        files.push(new);
        assert_eq!(changed_files(&files, Some(&review)).len(), 2);
        files[0].skip_reason_es = Some("Generado".into());
        assert_eq!(changed_files(&files, Some(&review))[0].path, "new.rb");
        assert!(changed_files(&[], Some(&review)).is_empty());
    }
    #[test]
    fn validates_contract_and_actual_line_locations() {
        let (target, files, r) = fixture();
        validate(&r, &target, &files, "quick").unwrap();
        let encoded = serde_json::to_string(&r).unwrap();
        validate(&parse_reply(&encoded).unwrap(), &target, &files, "quick").unwrap();
        let mut bad = r.clone();
        bad.files[0].evals[0].start = 0;
        assert!(validate(&bad, &target, &files, "quick").is_err());
        let mut bad = r.clone();
        bad.files[0].skipped = true;
        bad.files[0].skip_reason_es = Some("No lo expliqué".into());
        bad.files[0].parts.clear();
        bad.files[0].evals.clear();
        assert!(validate(&bad, &target, &files, "quick").is_err());
        let mut bad = r.clone();
        bad.files[0].parts[0].end = 40;
        assert!(validate(&bad, &target, &files, "quick").is_err());
        let mut bad = r.clone();
        bad.files[0].evals[0].kind = "check".into();
        assert!(validate(&bad, &target, &files, "quick").is_err());
        let mut bad = r.clone();
        bad.target.head_sha = "3".repeat(40);
        assert!(validate(&bad, &target, &files, "quick").is_err());
        let mut bad = r.clone();
        bad.files.push(bad.files[0].clone());
        assert!(validate(&bad, &target, &files, "quick").is_err());
        let mut bad = r.clone();
        bad.files[0].evals[0].severity = "warn".into();
        bad.files[0].evals[0].kind = "bug".into();
        assert!(validate(&bad, &target, &files, "quick").is_err());
        let mut v: Value = serde_json::from_str(&encoded).unwrap();
        v.as_object_mut().unwrap().remove("usage");
        assert!(parse_reply(&v.to_string()).is_err());
    }
    #[test]
    fn merge_base_excludes_newer_default_branch_commits() {
        let root = std::env::temp_dir().join(format!("flock-merge-base-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let g = |args: &[&str]| run(&root, "git", args, &[]).unwrap();
        g(&["init", "-b", "main"]);
        g(&["config", "user.email", "test@example.invalid"]);
        g(&["config", "user.name", "Flock test"]);
        std::fs::write(root.join("common"), "base\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-m", "base"]);
        let base = head(&root).unwrap();
        g(&["checkout", "-b", "feature"]);
        std::fs::write(root.join("feature"), "change\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-m", "feature"]);
        g(&["checkout", "main"]);
        std::fs::write(root.join("new-on-main"), "unrelated\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-m", "new main commit"]);
        g(&["remote", "add", "origin", root.to_str().unwrap()]);
        g(&["fetch", "origin"]);
        g(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        g(&["checkout", "feature"]);
        let found = merge_base(&root, &[]).unwrap();
        assert_eq!(found, base);
        let files = diff_between(&root, &found, &head(&root).unwrap()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "feature");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    fn small_worktree(root: &Path) -> (AppState, Worktree) {
        let repo = root.join("repo");
        let checkout = root.join("worktree");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |args: &[&str]| run(&repo, "git", args, &[]).unwrap();
        g(&["init", "-b", "main"]);
        g(&["config", "user.name", "Flock smoke"]);
        g(&["config", "user.email", "test@example.invalid"]);
        std::fs::write(
            repo.join("availability.rb"),
            "class Availability\n  def available?(open)\n    open\n  end\nend\n",
        )
        .unwrap();
        g(&["add", "."]);
        g(&["commit", "-m", "base"]);
        g(&["remote", "add", "origin", repo.to_str().unwrap()]);
        g(&["fetch", "origin"]);
        g(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        g(&[
            "worktree",
            "add",
            "-b",
            "pause-orders",
            checkout.to_str().unwrap(),
        ]);
        std::fs::write(checkout.join("availability.rb"),"class Availability\n  # Returns whether ordering is available\n  def available?(open, paused: false)\n    open && !paused\n  end\nend\n").unwrap();
        run(&checkout, "git", &["add", "."], &[]).unwrap();
        run(
            &checkout,
            "git",
            &["commit", "-m", "Let a location pause new orders"],
            &[],
        )
        .unwrap();
        let db = crate::db::Db::open_at(&root.join("smoke.db")).unwrap();
        let r = db
            .insert_repo("revisar-smoke", repo.to_str().unwrap())
            .unwrap();
        let w = db
            .insert_worktree(
                r.id,
                "pause-orders",
                checkout.to_str().unwrap(),
                None,
                "default",
                "worktree",
                None,
                Some("Flock"),
                None,
                None,
                "claude",
            )
            .unwrap();
        let state = AppState {
            db,
            pty: crate::pty::PtyManager::new(),
            statuses: Arc::new(Mutex::new(HashMap::new())),
            remote: Mutex::new(None),
            kb_watcher: Mutex::new(None),
            active_worktree: Mutex::new(None),
            input_locks: Mutex::new(HashMap::new()),
            remove_confirms: crate::confirm::PendingConfirms::default(),
        };
        (state, w)
    }
    /// Opt-in only: spends real Sonnet tokens. Isolated DB, checkout and cache.
    #[test]
    #[ignore = "real Claude call; run explicitly with --ignored --nocapture"]
    fn real_small_worktree() {
        let root = data_dir()
            .unwrap()
            .join(format!("smoke-{}", rand::random::<u64>()));
        let (state, w) = small_worktree(&root);
        let view = generate_at(&state, w.id, &root.join("cache")).unwrap();
        let review = view.review.unwrap();
        println!(
            "SMOKE_REVIEW={}\nUSAGE={}",
            root.display(),
            serde_json::to_string(&review.usage).unwrap()
        );
        let cached = generate_at(&state, w.id, &root.join("cache")).unwrap();
        assert!(cached.cached);
        assert_eq!(cached.review.unwrap().usage.cost_usd, review.usage.cost_usd);
    }
    #[test]
    fn validates_deep_exports_and_exposes_errors_without_generating() {
        let root =
            std::env::temp_dir().join(format!("flock-deep-export-{}", rand::random::<u64>()));
        let (state, w) = small_worktree(&root);
        let cwd = Path::new(&w.path);
        let base = merge_base(cwd, &[]).unwrap();
        let sha = head(cwd).unwrap();
        let files = diff_between(cwd, &base, &sha).unwrap();
        let example = include_str!("../../docs/review-format.md")
            .split("```json\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        let mut r: Review = serde_json::from_str(example).unwrap();
        r.kind = "deep".into();
        r.usage.model = "opus".into();
        r.target = Target {
            repo: "revisar-smoke".into(),
            own: true,
            worktree_id: w.id,
            pr_url: None,
            base_sha: base,
            head_sha: sha.clone(),
        };
        r.files[0].path = files[0].path.clone();
        r.files[0].diff_hash = files[0].diff_hash.clone();
        r.files[0].parts[0].start = 3;
        r.files[0].parts[0].end = 4;
        r.files[0].evals[0].start = 2;
        r.files[0].evals[0].end = 2;
        let cache = root.join("cache");
        let path = cache_path(&cache, w.id, &sha, true).unwrap();
        write_json(&path, &r).unwrap();
        write_json(&path.with_extension("target.json"), &r.target).unwrap();
        let view = load_at(&state, w.id, &cache, true).unwrap();
        assert!(view.deep_available);
        assert_eq!(view.review.unwrap().kind, "deep");
        r.version = 2;
        write_json(&path, &r).unwrap();
        let view = load_at(&state, w.id, &cache, false).unwrap();
        assert!(!view.deep_available);
        assert!(view.deep_error.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
