# Flock MCP server

Lets an agent drive Flock the way you do — spawn tasks, read status, send
input, and schedule work — by exposing Flock's REST API as MCP tools. This is
the substrate for "an agent that orchestrates other agents."

It's a thin **stdio bridge**: no orchestration logic lives here, it just calls
Flock's local REST API.

## Setup

1. Turn on **Local API (Flock MCP)** in Flock → Settings (this starts the API server).
2. Install deps once:
   ```bash
   cd mcp && npm install
   ```
3. Sessions Flock launches get it automatically: Flock installs a copy in its
   data dir and passes it to every Claude (`--mcp-config`) and Codex
   (`-c mcp_servers.flock.*`) session it starts, whatever the profile's own
   config says. For an agent outside Flock, add it yourself (e.g. Claude Code):
   ```bash
   claude mcp add flock -- node /absolute/path/to/flock/mcp/flock-mcp.mjs
   ```

The server reads Flock's API token from the data dir automatically. Override
with `FLOCK_TOKEN` / `FLOCK_API_URL` env vars if needed.

## Tools

| Tool              | What it does                                                        |
| ----------------- | ------------------------------------------------------------------- |
| `task_create`     | Create a worktree + start Claude or Codex with a prompt (`agent`, `model`, `effort` required) |
| `task_list`       | List worktrees with live status (working / idle / needs_input)      |
| `task_status`     | Counts of agents by status                                          |
| `task_read`       | Read an agent's conversation transcript (follow what a child is doing) |
| `task_input`      | Send text or a key (enter/escape/tab/arrows/ctrl-c) to an agent     |
| `notify_orchestrator` | From a child: message the orchestrator that spawned it (`done` / `blocked` / `question` / `info`) |
| `schedule_create` | Schedule a recurring task (`@every 30m` / `@every 1d` / `HH:MM`; `agent`, `model`, `effort` required) |
| `schedule_list`   | List scheduled tasks (with each one's agent/model/effort)          |
| `kb_search`       | Search the knowledge base (Obsidian vault) — your durable memory    |
| `kb_read`         | Read a note by vault-relative path                                  |
| `kb_list`         | List notes, optionally filtered by a path prefix                    |
| `kb_ingest`       | Write/update a note (and the vault file) — save durable learnings   |
| `kb_delete`       | Delete a note from the index and the vault                          |

An orchestrator agent can `task_create` to fan work out across repos, poll
`task_list` / `task_status` to watch progress, `task_read` to follow what each
child is doing, and `task_input` to nudge one that's waiting. When the agent runs
inside a Flock session, `task_create` auto-links the new worktree to it (via the
injected `FLOCK_WORKTREE_ID`) so it shows up as part of that orchestrator's fleet
— see Flock's first-class **Orchestrator** sessions. Going the other way, a child
calls `notify_orchestrator` when it finishes, gets blocked or needs a decision:
Flock looks its orchestrator up and submits the message there as
`[Flock · task #N "title" (repo) · kind] …`. Orchestrator-spawned children are
told about the tool in their system prompt.

## Knowledge base

`kb_*` exposes an Obsidian vault (set its path in Flock → Settings) as durable,
cross-session memory: the vault on disk is the source of truth, indexed into
SQLite FTS5 and kept fresh as you edit notes in Obsidian. The server's
instructions tell the agent to `kb_search` for context before working and
`kb_ingest` learnings back as linked markdown notes — so the knowledge graph
grows from your sessions.

### PR explanations

Speak Spanish and refer to the numbered evaluations. When the user says “mandale el 2 y el 5” on her own PR, forward those findings' `fix_es` texts and `path:line` with `task_input(submit:true)`; inside that worktree, make the changes directly. Cached explanations cost nothing, and parts/flows are omitted from `review_get` unless explicitly requested.

All Revisar actions happen in chat; the tab is read-only. The complete tools are:

| Tool | Inputs | Result / action |
| --- | --- | --- |
| `review_open` | exactly one of `worktree_id`, `pr_url`; `generate` defaults true | Focus, generate or incrementally update. Cached opens cost nothing; returns status, eval_count, cost_usd. A URL creates a review-only worktree with no author agent. |
| `review_get` | exactly one target; `include_context` defaults false | Cached verdict and numbered findings from the displayed quick/deep snapshot. No model call. |
| `review_post_preview` | exactly one target; `event`, `body_en`, `comments` | Freeze validated English text and line mappings; return rendered text and preview_id. |
| `review_post` | `preview_id` only | Post the exact immutable preview, once, before its 30-minute expiry. |
| `review_deep` | exactly one target | Start the separate interactive Opus review and display its JSON export when ready. |

For publication, comments use either `{n, text_en}` (a displayed evaluation number) or `{path, side, start, end, text_en}` with real file line numbers and `LEFT`/`RIGHT`. Events are `COMMENT`, `REQUEST_CHANGES`, or `APPROVE`. The chatting agent translates Spanish itself into short, human English. **Before review_post, show the exact English body and every inline comment with a Spanish gloss under each, and obtain an explicit yes in chat.** Preparing, previewing or editing is not approval. Edits or an expired/changed preview need a new preview and a new yes. If a posting response is ambiguous, inspect GitHub before trying again.

Before `review_deep`, warn the user in Spanish that it costs much more than the quick review and **get an explicit yes first**. Never start it automatically. The interactive command is `/review-pr <pr_url> --flock-json <Flock data>/reviews/<worktree_id>/<head_sha>.deep.json`; the shared contract is documented in [docs/review-format.md](../docs/review-format.md).

The REST routes are POST `/api/review/open`, `/api/review/get`, `/api/review/post-preview`, `/api/review/post`, and `/api/review/deep`, with the same JSON inputs and normal Flock bearer authentication. Publishing honors the repository/worktree's resolved `GH_CONFIG_DIR`.
