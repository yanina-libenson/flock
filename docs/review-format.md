# Flock review format v1

Quick explanations and `/review-pr` deep reviews share this JSON contract. All fields are required. Spanish uses plain rioplatense words; GitHub comments are short English notes with a faithful Spanish gloss. `severity` is `warn`, `info` or `ok`; `kind` is `comment`, `naming`, `logic`, `test`, `scope`, `bug` or `ok`. A file without findings has exactly one `ok` evaluation.

```json
{
  "version": 1,
  "kind": "quick",
  "target": {
    "repo": "acme/web", "own": true, "worktree_id": 312, "pr_url": null,
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
    "parts": [{ "name_es": "¿Está pausado ahora?", "code_name": "orders_paused?", "what_es": "Mira si todavía falta para que termine la pausa. Si ya terminó, permite pedir de nuevo.", "side": "RIGHT", "start": 12, "end": 14 }],
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

## Chat tools and publication

All actions happen in chat. The tab only presents explanations, code, numbered assessments and hover highlighting. It has no generation, feedback, posting, or deep-review buttons. When both cached kinds exist, the text tabs Rápida / Profunda choose the displayed snapshot; `review_get` uses that same selection and numbering.

| MCP tool | REST route | Behavior |
| --- | --- | --- |
| `review_open({worktree_id} \| {pr_url}, generate=true)` | POST `/api/review/open` | Focus the tab; generate only for an uncached commit, or incrementally update changed files after new commits. Returns status, eval_count and cost_usd. `generate:false` never calls a model. |
| `review_get({worktree_id} \| {pr_url}, include_context=false)` | POST `/api/review/get` | Read the displayed cached verdict and numbered findings. No generation; parts and flows only on request. |
| `review_post_preview({worktree_id} \| {pr_url}, event, body_en, comments)` | POST `/api/review/post-preview` | Validate every comment against the diff and freeze the exact English payload. Return its rendered text, expiry, and preview_id. |
| `review_post(preview_id)` | POST `/api/review/post` | Publish exactly the stored payload, using the worktree's resolved environment (including GH_CONFIG_DIR). No text or event can be supplied at this step. |
| `review_deep({worktree_id} \| {pr_url})` | POST `/api/review/deep` | Start a separate interactive Opus session after the user explicitly agrees to the greater cost. |

Supply exactly one target: `worktree_id` or `pr_url`. Opening a URL creates a detached review-only worktree when its repository is registered; otherwise it snapshots `gh pr diff` in Flock's data directory without cloning a repository. These worktrees have no author-agent session or Terminal tab and can be removed normally. When an orchestrator opens the URL, the MCP sends its `FLOCK_WORKTREE_ID` as `parent_id` and a newly created worktree nests under that orchestrator in the sidebar; one that already exists keeps its current parent, and opening by `worktree_id` never changes it. In `task_list` such a child has `status: null` and `has_session: false`, and `task_input` to it is refused as read-only. Quick explanations use `target.own:false`. The registered repository's environment profile is reused; an unregistered repository uses the default profile resolved for Flock's review scratch area.

Preview comments are either `{"n":3,"text_en":"Could we drop this comment?"}` or `{"path":"app/models/location.rb","side":"RIGHT","start":11,"end":11,"text_en":"Could we drop this comment?"}`. Events are `COMMENT`, `REQUEST_CHANGES`, or `APPROVE`. A multi-line comment maps to GitHub's `line` = end, `side`, `start_line` = start, and `start_side`; a single line omits the start fields. Ranges must lie within one visible diff hunk on the given side. Numbered comments must belong to the displayed cached commit. The exact payload also pins GitHub's `commit_id`.

The chatting agent translates the user's Spanish into short, human English itself. It must show the **exact final English general comment and every inline comment**, with a Spanish gloss under each, and obtain an **explicit yes in chat** before calling `review_post`. Preparing or editing a review is not approval. A change to the text requires a new preview and another yes. There is no separate Sonnet translation call, draft editor, or feedback tray.

Previews are immutable through the API, expire in 30 minutes, and are consumed before the one network posting attempt. Concurrent or repeated uses cannot post again. If GitHub's response is ambiguous, inspect the PR before creating another preview. Publishing checks that the PR's head and base commits still match the preview; if they moved, create a new preview and obtain approval again. Preview payloads and posting results live under `<Flock data>/reviews/previews/`, never in the repo.

`review_deep` requires a warning in Spanish that Opus costs much more than the quick explanation, and an explicit yes **before** starting it. It runs `/review-pr <pr_url> --flock-json <absolute-path>` in a fresh interactive session in that worktree, separate from the author's session. The output remains `<Flock data>/reviews/<worktree_id>/<head_sha>.deep.json`, with a companion `.deep.target.json` containing the expected target. The watcher validates the export and displays it automatically when ready. Reopening its terminal only attaches an existing session; it never restarts a model. The skill-side export remains a separate implementation.
