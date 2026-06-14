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
| `task_input`      | Send text or a key (enter/escape/tab/arrows/ctrl-c) to an agent     |
| `schedule_create` | Schedule a recurring task (`@every 30m` / `@every 1d` / `HH:MM`)    |
| `schedule_list`   | List scheduled tasks                                                |

An orchestrator agent can `task_create` to fan work out, poll `task_list` to
watch progress, and `task_input` to nudge an agent that's waiting.
