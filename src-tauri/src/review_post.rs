//! A preview is immutable and single-use. Chat agents must obtain an explicit yes before posting.
use crate::{
    error::{AppError, AppResult},
    review::{self, DiffFile},
    review_access,
    review_github::{self, PrRef},
    state::AppState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
};
use tauri::{AppHandle, Manager};
#[derive(Deserialize)]
pub struct PreviewComment {
    pub n: Option<usize>,
    pub path: Option<String>,
    pub side: Option<String>,
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub text_en: String,
}
#[derive(Deserialize)]
pub struct PreviewRequest {
    pub worktree_id: Option<i64>,
    pub pr_url: Option<String>,
    pub event: String,
    pub body_en: String,
    pub comments: Vec<PreviewComment>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Preview {
    pub preview_id: String,
    pub worktree_id: i64,
    pub pr_url: String,
    pub expires_at: String,
    pub payload: Value,
    pub rendered: String,
    created_at: i64,
    expires_unix: i64,
    used_at: Option<i64>,
    base_ref_oid: String,
    pr: PrRef,
    payload_json: String,
}
#[derive(Serialize)]
pub struct PreviewShown {
    pub preview_id: String,
    pub pr_url: String,
    pub expires_at: String,
    pub rendered: String,
}
#[derive(Serialize)]
pub struct Posted {
    pub preview_id: String,
    pub url: String,
}
fn preview_path(root: &Path, id: &str) -> AppResult<PathBuf> {
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::msg("El preview_id no es válido"));
    }
    Ok(root.join("previews").join(format!("{id}.json")))
}
fn comment_range(file: &DiffFile, side: &str, start: u32, end: u32) -> AppResult<()> {
    review::check_range(file, side, start, end)?;
    let mut hunk = 0;
    let mut from = None;
    let mut to = None;
    for l in &file.lines {
        if l.kind == "hunk" {
            hunk += 1;
        }
        let n = if side == "RIGHT" { l.right } else { l.left };
        if n == Some(start) {
            from = Some(hunk);
        }
        if n == Some(end) {
            to = Some(hunk);
        }
    }
    if from != to {
        return Err(AppError::msg(
            "El comentario abarca dos bloques distintos del diff. Separalo en dos.",
        ));
    }
    Ok(())
}
fn mapped_comments(
    request: &PreviewRequest,
    view: Option<&review_access::CompactReview>,
    files: &[DiffFile],
    head: &str,
) -> AppResult<Vec<Value>> {
    let mut comments = Vec::new();
    for c in &request.comments {
        if c.text_en.trim().is_empty() {
            return Err(AppError::msg(
                "Cada comentario necesita su texto final en inglés",
            ));
        }
        let (path, side, start, end) = if let Some(n) = c.n {
            if c.path.is_some() || c.side.is_some() || c.start.is_some() || c.end.is_some() {
                return Err(AppError::msg(
                    "Usá un número de evaluación o una ubicación, no ambos",
                ));
            }
            let r = view.ok_or_else(|| {
                AppError::msg("Pedí review_get antes de usar números de evaluación")
            })?;
            if r.head_sha != head || r.stale {
                return Err(AppError::msg(
                    "Las evaluaciones son de otro commit. Pedí que actualicen la explicación.",
                ));
            }
            let e = r
                .evals
                .iter()
                .find(|e| e.n == n)
                .ok_or_else(|| AppError::msg(format!("No hay evaluación {n}")))?;
            (e.path.clone(), e.side.clone(), e.start, e.end)
        } else {
            (
                c.path
                    .clone()
                    .ok_or_else(|| AppError::msg("Falta el archivo"))?,
                c.side
                    .clone()
                    .ok_or_else(|| AppError::msg("Falta el lado de la línea"))?,
                c.start
                    .ok_or_else(|| AppError::msg("Falta la línea inicial"))?,
                c.end.ok_or_else(|| AppError::msg("Falta la línea final"))?,
            )
        };
        let f = files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| AppError::msg(format!("El archivo {path} no está en el PR")))?;
        comment_range(f, &side, start, end)?;
        let mut comment = json!({"path":path,"line":end,"side":side,"body":c.text_en});
        if start != end {
            comment["start_line"] = json!(start);
            comment["start_side"] = json!(side);
        }
        comments.push(comment);
    }
    Ok(comments)
}
fn render_payload(p: &Value) -> String {
    let mut text = format!(
        "{} · commit {}\n\n{}",
        p["event"].as_str().unwrap_or(""),
        p["commit_id"].as_str().unwrap_or(""),
        p["body"].as_str().unwrap_or("")
    );
    if let Some(comments) = p["comments"].as_array() {
        for (index, c) in comments.iter().enumerate() {
            let range = if let Some(start) = c["start_line"].as_u64() {
                format!("{start}–{}", c["line"])
            } else {
                c["line"].to_string()
            };
            text.push_str(&format!(
                "\n\n{}. {}:{} ({})\n{}",
                index + 1,
                c["path"].as_str().unwrap_or(""),
                range,
                c["side"].as_str().unwrap_or(""),
                c["body"].as_str().unwrap_or("")
            ));
        }
    }
    text
}
pub fn preview(app: &AppHandle, request: PreviewRequest) -> AppResult<PreviewShown> {
    if !["COMMENT", "REQUEST_CHANGES", "APPROVE"].contains(&request.event.as_str()) {
        return Err(AppError::msg(
            "El evento debe ser COMMENT, REQUEST_CHANGES o APPROVE",
        ));
    }
    if (request.event == "REQUEST_CHANGES"
        || (request.event == "COMMENT" && request.comments.is_empty()))
        && request.body_en.trim().is_empty()
    {
        return Err(AppError::msg("Escribí el comentario general en inglés"));
    }
    let state = app.state::<AppState>();
    let w = match (&request.worktree_id, &request.pr_url) {
        (None, Some(url)) => match review_github::find(&state, url)? {
            Some(w) => w,
            None => review_github::open_pr(app, url, None)?,
        },
        _ => review_github::resolve_cached(&state, request.worktree_id, request.pr_url.clone())?,
    };
    let (target, pr, files, base_ref_oid) = review_github::posting_target(&state, &w)?;
    let compact = if request.comments.iter().any(|c| c.n.is_some()) {
        Some(review_access::get(&state, w.id, false)?)
    } else {
        None
    };
    let comments = mapped_comments(&request, compact.as_ref(), &files, &target.head_sha)?;
    let payload = json!({"commit_id":target.head_sha,"event":request.event,"body":request.body_en,"comments":comments});
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::minutes(30);
    let p = Preview {
        preview_id: format!(
            "{:016x}{:016x}",
            rand::random::<u64>(),
            rand::random::<u64>()
        ),
        worktree_id: w.id,
        pr_url: pr.url.clone(),
        expires_at: expires.to_rfc3339(),
        rendered: render_payload(&payload),
        payload_json: payload.to_string(),
        payload,
        created_at: now.timestamp(),
        expires_unix: expires.timestamp(),
        used_at: None,
        base_ref_oid,
        pr,
    };
    review::write_json(&preview_path(&review::data_dir()?, &p.preview_id)?, &p)?;
    Ok(PreviewShown {
        preview_id: p.preview_id,
        pr_url: p.pr_url,
        expires_at: p.expires_at,
        rendered: p.rendered,
    })
}
fn ensure_unused(p: &Preview, now: i64) -> AppResult<()> {
    if p.used_at.is_some() {
        return Err(AppError::msg("Este preview ya se usó. Pedí uno nuevo."));
    }
    if now >= p.expires_unix || now < p.created_at {
        return Err(AppError::msg(
            "El preview venció. Pedí uno nuevo y confirmá el texto de nuevo.",
        ));
    }
    Ok(())
}
/// Deliberately takes only the stored id: a confirmed payload cannot be edited at posting time.
pub fn post(state: &AppState, preview_id: &str) -> AppResult<Posted> {
    static POST: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = POST.get_or_init(Default::default).lock().unwrap();
    let path = preview_path(&review::data_dir()?, preview_id)?;
    let mut p: Preview =
        serde_json::from_slice(&std::fs::read(&path)?).map_err(|e| AppError::msg(e.to_string()))?;
    ensure_unused(&p, chrono::Utc::now().timestamp())?;
    let w = state.db.get_worktree(p.worktree_id)?;
    let env = review::worktree_env(state, &w)?;
    let cwd = Path::new(&w.path);
    let info = review_github::metadata(cwd, &p.pr, &env)?;
    if info["headRefOid"] != p.payload["commit_id"]
        || info["baseRefOid"].as_str() != Some(&p.base_ref_oid)
    {
        return Err(AppError::msg(
            "El PR cambió después del preview. Pedí otro y confirmá el texto de nuevo.",
        ));
    }
    // Claim before the network call. Even an ambiguous transport failure cannot double-post this id.
    p.used_at = Some(chrono::Utc::now().timestamp());
    review::write_json(&path, &p)?;
    let mut child = Command::new(review::binary("gh")?)
        .current_dir(cwd)
        .envs(env)
        .args([
            "api",
            &format!(
                "repos/{}/{}/pulls/{}/reviews",
                p.pr.owner, p.pr.repo, p.pr.number
            ),
            "--method",
            "POST",
            "--input",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(p.payload_json.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(AppError::msg(format!("GitHub no confirmó la publicación. Este preview quedó usado; revisá el PR antes de pedir otro: {}",String::from_utf8_lossy(&out.stderr))));
    }
    let response: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| AppError::msg(e.to_string()))?;
    review::write_json(&path.with_extension("posted.json"), &response)?;
    Ok(Posted {
        preview_id: preview_id.into(),
        url: response["html_url"].as_str().unwrap_or(&p.pr_url).into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn files() -> Vec<DiffFile> {
        review::parse_diff("diff --git a/a.rb b/a.rb\n--- a/a.rb\n+++ b/a.rb\n@@ -10,2 +10,2 @@\n-old\n-older\n+new\n+newer\n@@ -40 +40 @@\n-last\n+final\n")
    }
    fn compact() -> review_access::CompactReview {
        review_access::CompactReview {
            worktree_id: 1,
            head_sha: "a".repeat(40),
            kind: "quick".into(),
            stale: false,
            verdict_es: "Hay algo para revisar.".into(),
            evals: vec![review_access::NumberedEval {
                n: 1,
                path: "a.rb".into(),
                side: "LEFT".into(),
                start: 10,
                end: 11,
                severity: "warn".into(),
                kind: "comment".into(),
                text_es: "Sobra.".into(),
                fix_es: "Sacalo.".into(),
                comment_en: "Could we drop this?".into(),
                comment_es: "¿Lo sacamos?".into(),
            }],
            cost_usd: 0.,
            summary: None,
            files: None,
        }
    }
    fn request() -> PreviewRequest {
        PreviewRequest {
            worktree_id: Some(1),
            pr_url: None,
            event: "COMMENT".into(),
            body_en: "A couple of notes.".into(),
            comments: vec![PreviewComment {
                n: Some(1),
                path: None,
                side: None,
                start: None,
                end: None,
                text_en: "Could we drop this?\nIt repeats the code.".into(),
            }],
        }
    }
    #[test]
    fn maps_number_to_old_multiline_github_location_and_preserves_english() {
        let r = request();
        let result = mapped_comments(&r, Some(&compact()), &files(), &"a".repeat(40)).unwrap();
        assert_eq!(
            result[0],
            json!({"path":"a.rb","line":11,"side":"LEFT","start_line":10,"start_side":"LEFT","body":"Could we drop this?\nIt repeats the code."})
        );
        let p = json!({"commit_id":"a".repeat(40),"event":"COMMENT","body":r.body_en,"comments":result});
        let rendered = render_payload(&p);
        assert!(rendered.contains("a.rb:10–11 (LEFT)"));
        assert!(rendered.contains("Could we drop this?\nIt repeats the code."));
    }
    #[test]
    fn rejects_stale_unknown_mixed_and_invalid_locations() {
        let mut c = compact();
        c.stale = true;
        assert!(mapped_comments(&request(), Some(&c), &files(), &c.head_sha).is_err());
        let mut r = request();
        r.comments[0].n = Some(2);
        assert!(mapped_comments(&r, Some(&compact()), &files(), &"a".repeat(40)).is_err());
        let mut r = request();
        r.comments[0].path = Some("a.rb".into());
        assert!(mapped_comments(&r, Some(&compact()), &files(), &"a".repeat(40)).is_err());
        assert!(comment_range(&files()[0], "RIGHT", 10, 40).is_err());
        assert!(comment_range(&files()[0], "LEFT", 0, 0).is_err());
        assert!(comment_range(&files()[0], "RIGHT", 12, 12).is_err());
        let mut r = request();
        r.comments[0] = PreviewComment {
            n: None,
            path: Some("a.rb".into()),
            side: Some("RIGHT".into()),
            start: Some(10),
            end: Some(10),
            text_en: "One note.".into(),
        };
        let result = mapped_comments(&r, None, &files(), &"a".repeat(40)).unwrap();
        assert!(result[0].get("start_line").is_none());
        assert_eq!(result[0]["side"], "RIGHT");
    }
    #[test]
    fn preview_is_immutable_expiring_single_use_and_survives_reopen() {
        let root = std::env::temp_dir().join(format!("flock-preview-{}", rand::random::<u64>()));
        let id = "a".repeat(32);
        let payload = json!({"commit_id":"b".repeat(40),"event":"APPROVE","body":"Looks good.","comments":[]});
        let mut p = Preview {
            preview_id: id.clone(),
            worktree_id: 1,
            pr_url: "https://github.com/o/r/pull/1".into(),
            expires_at: "2026-10-28T12:30:00Z".into(),
            payload: payload.clone(),
            rendered: render_payload(&payload),
            created_at: 100,
            expires_unix: 1900,
            used_at: None,
            base_ref_oid: "c".repeat(40),
            pr: review_github::parse_url("https://github.com/o/r/pull/1").unwrap(),
            payload_json: payload.to_string(),
        };
        let path = preview_path(&root, &id).unwrap();
        review::write_json(&path, &p).unwrap();
        let saved: Preview = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.payload_json, payload.to_string());
        ensure_unused(&saved, 1899).unwrap();
        assert!(ensure_unused(&saved, 1900).is_err());
        assert!(ensure_unused(&saved, 99).is_err());
        p.used_at = Some(101);
        assert!(ensure_unused(&p, 102).is_err());
        assert!(preview_path(&root, "../../outside").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
