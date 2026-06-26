<div align="center">

# 🐦‍⬛ Flock

### Run a whole flock of Claude Code agents in parallel — one git worktree each, all in one window.

[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24C8DB?logo=tauri&logoColor=white)](https://tauri.app)
[![SolidJS](https://img.shields.io/badge/SolidJS-2C4F7C?logo=solid&logoColor=white)](https://solidjs.com)
[![Rust](https://img.shields.io/badge/Rust-000000?logo=rust&logoColor=white)](https://rust-lang.org)
[![macOS](https://img.shields.io/badge/macOS-Apple%20Silicon-000000?logo=apple&logoColor=white)](#requirements)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](#license)

**Flock turns "I wish I could have ten Claudes working at once" into a tool you actually want to live in.**

</div>

<!--
  Screenshots: drop images in docs/screenshots/ and embed them here, e.g.
  ![Flock sidebar with per-worktree task status](docs/screenshots/sidebar.png)
  Curate them first — make sure no private repo/branch names are visible.
-->

---

## What is Flock?

Flock is a macOS desktop app for running **many [Claude Code](https://claude.com/claude-code) sessions in parallel** — each in its own **git worktree**, each on its own branch, each a persistent terminal you can leave and come back to.

Instead of juggling tmux panes and a forest of terminal tabs, you get one calm window: a sidebar of repos and their worktrees, a tab bar of open sessions, and — at a glance — **whose turn it is on every single task**. Is Claude working? Waiting on you? Is the PR green and ready to merge, or stuck in review? Flock tells you without you having to look.

It's built to feel like a great terminal (it's modeled on iTerm down to the line height), but it knows what your agents are *doing*.

> Inspired by [drn/argus](https://github.com/drn/argus) — Flock takes the "watch your agents" idea and turns it into a full parallel-development cockpit.

---

## Why you'll want it

You're already running Claude Code. The bottleneck isn't the model — it's **you**, context-switching between tasks, forgetting which branch had the failing test, missing the moment an agent stopped to ask a question. Flock fixes the orchestration layer:

- **Parallelism without chaos.** Spin up a worktree per task. Each gets an isolated checkout and its own Claude. They don't step on each other.
- **Never miss "your turn."** A background monitor reads each session and tells you who needs you — on screen, and on your phone.
- **The ball is always visible.** Every worktree shows where its task sits in the PR lifecycle, so you know if you're blocked or blocking.
- **It survives everything.** Sessions live in tmux. Close the app, reboot the UI, come back tomorrow — your conversations are right where you left them.

---

## Features

### 🪟 Parallel worktree sessions
- One click spins up a **new git worktree** (new branch, isolated checkout) with a fresh Claude Code session inside it.
- Each session is a **tmux session** (`flock-<id>` on a dedicated `flock` socket), so it **persists across app restarts** — quit Flock, reopen, reattach.
- Default branch names are evocative world places (`kyoto`, `petra`, `ushuaia`…) so you're not naming branches at 2am.

### 🚦 Per-worktree task status — *is the ball on my end?*
A single status pill per worktree, color-coded by whose turn it is:
- **Working / Needs you** — live agent activity, read straight off the terminal.
- **Push PR · Monitoring CI · CI failed · Waiting for review · Comments to address · Changes requested · Conflicts · Ready to merge · Merged** — the full PR lifecycle, derived from `gh` (including unresolved review threads from humans *and* bots).
- Amber = your move · Blue = in progress / waiting on others · Green = ready to merge · Dim = no active loop.

### 👀 Live monitoring & notifications
- A backend monitor polls every session (~2s) via `tmux capture-pane` and classifies it as **working / idle / needs input** from the rendered screen.
- **Native notifications** when an agent asks a question or finishes a long task — tuned to avoid flicker, with click-to-jump back to the right worktree.
- `⌘J` cycles to the next agent that needs you.

### 🏷️ Auto-generated titles
- Each session gets a short, human title summarized by a one-shot `claude -p` (fast model) from what's on screen — so your tabs say *"Fix the checkout race"*, not `feat/kyoto`. Editable inline.

### 📱 Mobile access (PWA over Tailscale)
- An opt-in local server exposes your worktree list, **live terminals** (streamed to your phone and reflowed to its width), **remote input**, a **New Task** form, web push notifications, and a **Reader** chat view that renders the session transcript directly.
- Security-first: binds `127.0.0.1` + your Tailscale IP only (never `0.0.0.0`), every API route gated by a locally-generated master token.

### 🧠 Built-in knowledge base (durable agent memory)
- Point Flock at an **Obsidian vault**; it indexes your notes into **SQLite FTS5** and serves them over **MCP** (`kb_search`, `kb_read`, `kb_ingest`, …). Your agents get a persistent, searchable memory across sessions.

### 🤖 Orchestration & scheduling
- **MCP server** (`mcp/flock-mcp.mjs`): other agents (or Claude itself) can `task_create`, `task_list`, `task_status`, `task_input`, and manage schedules — a stdio bridge to Flock's REST API.
- **Scheduled tasks**: fire a fresh prompted task on a cron-like spec (`@every 30m`, `@every 2h`, `HH:MM`).
- **Headless task creation**: spawn a worktree + prompted Claude without ever touching the UI.

### ⚙️ Quality-of-life
- **Per-folder environment profiles** — inject env vars (API keys, etc.) by binding a directory to an environment; longest-prefix match, tokens stored `0600` outside any repo.
- **Memory-aware** — idle sessions hibernate and reattach on demand; a RAM budget reaps runaway sessions; panes attach lazily so launch doesn't spawn every Claude at once.
- **Drag a file onto the window** to insert its path into the active session.
- **iTerm-matched theme** — Monaco 12, classic ANSI palette, pure black, `⌘B` to toggle the sidebar.

---

## Requirements

Flock is a **macOS** app (Apple Silicon builds shipped; `aarch64`). It orchestrates tools you already have on your `PATH`:

| Tool | Why |
|------|-----|
| [`claude`](https://claude.com/claude-code) | the agent each worktree runs |
| `tmux` | session persistence (`brew install tmux`) |
| `gh` (authenticated) | per-worktree PR status (`brew install gh && gh auth login`) |
| `git` | worktrees |

For building from source you'll also need the [Rust toolchain](https://rustup.rs) and [Node.js](https://nodejs.org).

---

## Getting started

```bash
# install JS deps
npm install

# run in dev (hot-reload) — best for hacking on Flock itself
npm run tauri dev

# build a standalone .app + .dmg — for daily use
npm run tauri build
# → src-tauri/target/release/bundle/dmg/Flock_<version>_aarch64.dmg
#   open the dmg and drag Flock.app to /Applications
```

Then: add a repo (the sidebar `+`), create a worktree, and Claude starts in it. That's the loop.

---

## Architecture (for contributors **and** agents)

> 👋 **If you're an AI agent working in this repo, start here.** This section is the map.

**The model:** a **repo** has many **worktrees**; each worktree is a git branch + isolated checkout + one tmux session (`flock-<worktree_id>`) running `claude`. State lives in a local SQLite DB and in `~/Library/Application Support/Flock/` (or `~/.flock/`) — never in the repo.

**Stack:** [Tauri 2](https://tauri.app) (Rust backend) + [SolidJS](https://solidjs.com) + Vite + Tailwind v4 frontend, with [xterm.js](https://xtermjs.org) (DOM renderer — WebGL corrupts inside the Tauri webview) for the terminal.

### Backend — `src-tauri/src/`

| File | Responsibility |
|------|----------------|
| `lib.rs` | app setup; registers Tauri commands and spawns the background threads (`monitor`, `pr`, `schedule`, kb indexer) |
| `commands.rs` | all `#[tauri::command]` IPC handlers the frontend calls via `invoke` |
| `git.rs` | git plumbing — worktree add/remove/list, branch detection, dirty/ahead checks |
| `pr.rs` | per-worktree PR lifecycle status, derived from `gh`; 60s background poller + on-demand command |
| `monitor.rs` | reads each tmux session (`capture-pane`), classifies working/idle/needs_input, emits status + auto-titles |
| `pty.rs` | tmux session lifecycle + the PTY client that bridges xterm ↔ tmux |
| `db.rs` | SQLite (repos, worktrees, schedules) |
| `api/mod.rs` | opt-in axum server: REST + SSE for the mobile PWA, token auth, Tailscale binding |
| `schedule.rs` | cron-spec task firing |
| `kb.rs` | Obsidian vault → FTS5 index, exposed over MCP |
| `env_profiles.rs` | per-folder env-var injection |
| `transcript.rs` | reads Claude session JSONL for the PWA Reader |

### Frontend — `src/`

| File | Responsibility |
|------|----------------|
| `App.tsx` | shell; wires backend events (`worktree:status`, `worktree:pr_status`, `worktree:title`, …) into the store |
| `lib/ipc.ts` | typed wrappers over every Tauri `invoke` + event listener (mirror of the Rust types) |
| `lib/store.ts` | SolidJS reactive store (repos, worktrees, open panes, per-worktree statuses) |
| `components/Sidebar.tsx` | the repo/worktree tree + the per-worktree **status pill** |
| `components/TabBar.tsx`, `TerminalPane.tsx`, `TitleBar.tsx` | tabs, the live terminal, the top bar |

### The bridges

- **`mcp/flock-mcp.mjs`** — MCP stdio server exposing `task_*`, `schedule_*`, `kb_*` to any agent. Talks to the REST API; reads the token from the data dir or `FLOCK_TOKEN` / `FLOCK_API_URL`.
- **`pwa/`** — the installable mobile shell (vanilla JS + xterm) served by `api/mod.rs`.

### Conventions worth keeping
- New frontend↔backend calls: add the command in `commands.rs`, register it in `lib.rs`'s `generate_handler!`, and add a typed wrapper in `lib/ipc.ts`.
- Background work that touches every worktree (not just live sessions) follows the `pr.rs` poller pattern; per-session screen work follows `monitor.rs`.
- GUI apps launch with a **minimal `PATH`** (no Homebrew) — resolve external binaries (`gh`, `claude`) by absolute path or via the login shell, as `pr.rs` / `monitor.rs` do.

```bash
# build + test the backend
cd src-tauri && cargo build && cargo test
# typecheck the frontend
npx tsc --noEmit
```

---

## License

[MIT](LICENSE) © Yanina Libenson

<div align="center">
<sub>Built for people who let agents do the typing — and want to see the whole flock at a glance.</sub>
</div>
