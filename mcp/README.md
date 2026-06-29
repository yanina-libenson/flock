# Flock MCP server

Lets an agent drive Flock the way you do — spawn tasks, read status, send
input, and schedule work — by exposing Flock's REST API as MCP tools. This is
the substrate for "an agent that orchestrates other agents."

It's a thin **stdio bridge**: no orchestration logic lives here, it just calls
the same REST API the PWA uses.

## Setup

1. Turn on **Remote access** in Flock → Settings (this starts the API server).
2. Install deps once:
   ```bash
   cd mcp && npm install
   ```
3. Add it to an agent (e.g. Claude Code):
   ```bash
   claude mcp add flock -- node /absolute/path/to/flock/mcp/flock-mcp.mjs
   ```

The server reads Flock's API token from the data dir automatically. Override
with `FLOCK_TOKEN` / `FLOCK_API_URL` env vars if needed.

## Tools

| Tool              | What it does                                                        |
| ----------------- | ------------------------------------------------------------------- |
| `task_create`     | Create a worktree + start Claude on it with an initial prompt       |
| `task_list`       | List worktrees with live status (working / idle / needs_input)      |
| `task_status`     | Counts of agents by status                                          |
| `task_read`       | Read an agent's conversation transcript (follow what a child is doing) |
| `task_input`      | Send text or a key (enter/escape/tab/arrows/ctrl-c) to an agent     |
| `schedule_create` | Schedule a recurring task (`@every 30m` / `@every 1d` / `HH:MM`)    |
| `schedule_list`   | List scheduled tasks                                                |
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
— see Flock's first-class **Orchestrator** sessions.

## Knowledge base

`kb_*` exposes an Obsidian vault (set its path in Flock → Settings) as durable,
cross-session memory: the vault on disk is the source of truth, indexed into
SQLite FTS5 and kept fresh as you edit notes in Obsidian. The server's
instructions tell the agent to `kb_search` for context before working and
`kb_ingest` learnings back as linked markdown notes — so the knowledge graph
grows from your sessions.
