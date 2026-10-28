# Flock review format v1

Quick explanations and `/review-pr` deep reviews share this JSON contract. All fields are required. Spanish uses plain rioplatense words; GitHub comments are short English notes with a faithful Spanish gloss. `severity` is `warn`, `info` or `ok`; `kind` is `comment`, `naming`, `logic`, `test`, `scope`, `bug` or `ok`. A file without findings has exactly one `ok` evaluation.

```json
{
  "version": 1,
  "kind": "quick",
  "target": {
    "repo": "thanx-ordering", "own": true, "worktree_id": 312, "pr_url": null,
    "base_sha": "1111111111111111111111111111111111111111",
    "head_sha": "2222222222222222222222222222222222222222"
  },
  "summary": {
    "what_es": "El comercio puede pausar los pedidos de un local. Mientras dura la pausa, los clientes no pueden pedir ahí. Los pedidos ya hechos siguen igual.",
    "flows": [{ "who_es": "Cuando el comercio pausa", "steps": [{ "label_es": "Dashboard", "changed": false }, { "label_es": "Local pausado", "changed": true }] }],
    "verdict_es": "Casi listo. Hay un comentario que se puede sacar."
  },
  "files": [{
    "path": "app/models/location.rb", "status": "modified",
    "diff_hash": "3333333333333333333333333333333333333333",
    "skipped": false, "skip_reason_es": null,
    "name_es": "Local (modelo)", "role_es": "Guarda si el local puede recibir pedidos.",
    "parts": [{ "name_es": "¿Está pausado ahora?", "code_name": "online_ordering_paused?", "what_es": "Mira si todavía falta para que termine la pausa. Si ya terminó, permite pedir de nuevo.", "side": "RIGHT", "start": 12, "end": 14 }],
    "evals": [{ "id": "f2e1", "severity": "warn", "kind": "comment", "side": "RIGHT", "start": 11, "end": 11,
      "text_es": "El comentario repite el nombre del método y no agrega información.",
      "fix_es": "Borrar el comentario.", "comment_en": "This comment repeats the method name. Could we drop it?",
      "comment_es": "El comentario repite el nombre del método. ¿Lo sacamos?" }]
  }],
  "usage": { "model": "sonnet", "input_tokens": 0, "output_tokens": 0, "cache_read_tokens": 0, "cost_usd": 0.0 },
  "generated_at": "2026-10-28T12:00:00Z"
}
```

`status`: `added|modified|deleted|renamed`. `diff_hash` is the hexadecimal SHA-1 of the complete file diff text, with LF endings and its final LF. `side`: `RIGHT` = new file, `LEFT` = old file (including deleted lines). `start` and `end` are inclusive, positive **file line numbers**, not positions in the diff. Both endpoints must appear in a hunk on that side. These locations map directly to GitHub review comments. Omitted files remain in `files` with `skipped: true`, a Spanish `skip_reason_es`, and empty `parts`/`evals`.

Quick reviews live outside the repo at `<Flock data>/reviews/<worktree_id>/<head_sha>.json`. The app owns usage from Claude's JSON envelope; it sums quick runs across incremental updates, includes cache creation in input tokens, and shows cache reads separately. `<head_sha>.run.json` tracks the last generation's bill, including a single format-repair attempt. Opening an existing review never calls the model. Updates send changed diffs and compact prior context, reuse unchanged file explanations, remove paths no longer in the PR, and regenerate the summary.

The deep-review integration uses an interactive Claude Opus session with first prompt:

```text
/review-pr <pr_url> --flock-json <absolute-path>
```

The output path is `<Flock data>/reviews/<worktree_id>/<head_sha>.deep.json`. The skill writes a complete format-v1 document with `kind: "deep"`, the matching target commits, hashes and real line locations. Write to a temporary file then rename it into place so the watcher only sees complete JSON. The app validates and renders the same contract. Implementing the skill's `--flock-json` export is outside this app's scope.

The Revisar tab is read-only. Opening, generating and updating are chat actions through MCP `review_open({worktree_id, generate:true})` (POST `/api/review/open`). `generate:false` only focuses the cached explanation. Empty and stale views ask the user to request the explanation or update in chat; the tab never generates text by itself.

Non-ok evaluations are numbered `1, 2, 3…` in cached file order, then evaluation order within each file. Ok evaluations have no number. The same numbers appear in the summary, cards, gutter and `review_get`; they stay stable for that review snapshot. Numbers are derived presentation data and are **not** added to the shared JSON contract. After updating commits, use the new snapshot's numbers.

`review_get({worktree_id, include_context:false})` (POST `/api/review/get`) reads the cache without generating. It returns `verdict_es` and compact `evals` with `n`, `path`, `side`, `start`, `end`, `severity`, `kind`, `text_es`, `fix_es`, `comment_en`, and `comment_es`, plus commit, stale flag and cost. Parts and flows are returned only with `include_context:true`. For an own PR, the orchestrator sends the chosen fixes with `task_input(submit:true)`; an agent already in that worktree makes the changes directly. There is no feedback tray or extra feedback-generation call.
