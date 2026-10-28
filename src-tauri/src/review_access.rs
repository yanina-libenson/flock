//! Compact, numbered findings for chat. Numbers follow the cached file/eval order.
use crate::{
    error::{AppError, AppResult},
    review::{Part, Review, ReviewView, Summary},
    state::AppState,
};
use serde::Serialize;
#[derive(Clone, Debug, Serialize)]
pub struct NumberedEval {
    pub n: usize,
    pub path: String,
    pub side: String,
    pub start: u32,
    pub end: u32,
    pub severity: String,
    pub kind: String,
    pub text_es: String,
    pub fix_es: String,
    pub comment_en: String,
    pub comment_es: String,
}
pub fn evals(r: &Review) -> Vec<NumberedEval> {
    let mut result = Vec::new();
    for f in &r.files {
        for e in &f.evals {
            if e.severity != "ok" {
                result.push(NumberedEval {
                    n: result.len() + 1,
                    path: f.path.clone(),
                    side: e.side.clone(),
                    start: e.start,
                    end: e.end,
                    severity: e.severity.clone(),
                    kind: e.kind.clone(),
                    text_es: e.text_es.clone(),
                    fix_es: e.fix_es.clone(),
                    comment_en: e.comment_en.clone(),
                    comment_es: e.comment_es.clone(),
                });
            }
        }
    }
    result
}
#[derive(Serialize)]
pub struct ContextFile {
    path: String,
    name_es: String,
    role_es: String,
    parts: Vec<Part>,
}
#[derive(Serialize)]
pub struct CompactReview {
    pub worktree_id: i64,
    pub head_sha: String,
    pub kind: String,
    pub stale: bool,
    pub verdict_es: String,
    pub evals: Vec<NumberedEval>,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<ContextFile>>,
}
pub fn compact(view: ReviewView, include_context: bool) -> AppResult<CompactReview> {
    let r = view.review.ok_or_else(|| {
        AppError::msg("Todavía no hay una explicación. Pedí que abran este PR con review_open.")
    })?;
    Ok(CompactReview {
        worktree_id: r.target.worktree_id,
        stale: r.target.head_sha != view.head_sha,
        head_sha: r.target.head_sha.clone(),
        kind: r.kind.clone(),
        verdict_es: r.summary.verdict_es.clone(),
        evals: evals(&r),
        cost_usd: r.usage.cost_usd,
        summary: include_context.then(|| r.summary.clone()),
        files: include_context.then(|| {
            r.files
                .iter()
                .map(|f| ContextFile {
                    path: f.path.clone(),
                    name_es: f.name_es.clone(),
                    role_es: f.role_es.clone(),
                    parts: f.parts.clone(),
                })
                .collect()
        }),
    })
}
pub fn get(state: &AppState, id: i64, include_context: bool) -> AppResult<CompactReview> {
    compact(crate::review::load_selected(state, id, None)?, include_context)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_skip_ok_evals_and_follow_file_order_without_sending_context() {
        let doc = include_str!("../../docs/review-format.md");
        let example = doc
            .split("```json\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        let mut r: Review = serde_json::from_str(example).unwrap();
        let mut ok = r.files[0].clone();
        ok.path = "before.rb".into();
        ok.evals[0].severity = "ok".into();
        ok.evals[0].kind = "ok".into();
        let mut second = r.files[0].clone();
        second.path = "second.rb".into();
        second.evals[0].id = "f3e1".into();
        r.files.insert(0, ok);
        r.files.push(second);
        let numbered = evals(&r);
        assert_eq!(numbered.len(), 2);
        assert_eq!(numbered[0].n, 1);
        assert_eq!(numbered[0].path, "app/models/location.rb");
        assert_eq!(numbered[1].n, 2);
        assert_eq!(numbered[1].path, "second.rb");
        assert_eq!(
            serde_json::to_value(evals(&r)).unwrap(),
            serde_json::to_value(numbered).unwrap()
        );
        let head = r.target.head_sha.clone();
        let v = ReviewView {
            worktree_id: 312,
            head_sha: head,
            review: Some(r),
            files: vec![],
            cached: true,
            last_cost_usd: 0.,
            error: None,
            deep_available: false,
            deep_error: None,
        };
        let json = serde_json::to_value(compact(v, false).unwrap()).unwrap();
        assert!(json.get("summary").is_none());
        assert!(json.get("files").is_none());
        assert_eq!(json["evals"][0]["n"], 1);
        assert!(json["evals"][0].get("id").is_none());
    }
}
