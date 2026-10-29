#!/usr/bin/env node
// Flock MCP server — lets an agent drive Flock the way you do.
//
// It's a thin stdio bridge to Flock's REST API (localhost only),
// so there's no orchestration logic duplicated here. Add it to an agent with:
//
//   claude mcp add flock -- node /absolute/path/to/flock-mcp.mjs
//
// Requires Flock's "Local API (Flock MCP)" toggle to be ON (it starts the API server).
// Reads the API token from Flock's data dir; override with FLOCK_TOKEN /
// FLOCK_API_URL env vars if needed.

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  ListToolsRequestSchema,
  CallToolRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const API_URL = process.env.FLOCK_API_URL || "http://127.0.0.1:7765";

function loadToken() {
  if (process.env.FLOCK_TOKEN) return process.env.FLOCK_TOKEN.trim();
  // macOS data dir; matches dirs::data_local_dir().join("Flock").
  const p = join(homedir(), "Library", "Application Support", "Flock", "api-token");
  try {
    return readFileSync(p, "utf8").trim();
  } catch {
    return "";
  }
}
const TOKEN = loadToken();

async function apiCall(method, path, body) {
  const res = await fetch(`${API_URL}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      "Content-Type": "application/json",
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`Flock API ${method} ${path} → ${res.status}: ${text}`);
  }
  return text ? safeParse(text) : null;
}
function safeParse(t) {
  try {
    return JSON.parse(t);
  } catch {
    return t;
  }
}

const TOOLS = [
  {
    name: "task_create",
    description:
      "Spawn a new agent task: create a git worktree in the given repo and start a coding agent (Claude Code or Codex — see `agent`) on it with an initial prompt. Returns the new worktree.",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string", description: "Repo name as registered in Flock" },
        prompt: { type: "string", description: "Initial prompt for the agent" },
        branch: { type: "string", description: "Optional branch leaf; auto-derived from the prompt if omitted" },
        base: { type: "string", description: "Optional base branch" },
        title: { type: "string", description: "Optional display title" },
        model: {
          type: "string",
          enum: [
            "opus",
            "sonnet",
            "haiku",
            "fable",
            "claude-opus-5-5",
            "claude-sonnet-5",
            "claude-haiku-4-5-20251001",
            "claude-fable-5-1",
            // Codex agents (agent: "codex"). "default" = Codex's configured model.
            "default",
            "gpt-6.1-sol",
            "gpt-6-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
          ],
          description:
            "REQUIRED. Model for this agent's session; must match the agent. Claude agents: an alias or full Claude model id — haiku = mechanical, sonnet = scoped/typical work, opus = ambiguous/high-stakes; fable ONLY if the user asks. Codex agents: \"default\" almost always; a gpt-… id only if the user names it. Always explicit.",
        },
        effort: {
          type: "string",
          enum: ["low", "medium", "high", "xhigh", "max", "default"],
          description: "REQUIRED. Reasoning effort for this agent's session. haiku→low; small scoped sonnet→medium; typical multi-file sonnet→high; opus→high, or xhigh for the hardest/highest-stakes; Codex: same difficulty ladder. `max` only if the user asks. If unsure, pick the higher one. \"default\" is accepted for Codex agents only.",
        },
        agent: {
          type: "string",
          enum: ["claude", "codex"],
          description:
            "REQUIRED. \"claude\" or \"codex\". Default claude; codex ONLY if the user asks for it, Claude is out of credit, or you are running as Codex. Never switch provider on your own initiative. Codex is only allowed in repos on the Thanx or Flock profile; Flock refuses otherwise.",
        },
        confirm_cross_account: {
          type: "boolean",
          description:
            "Flock refuses to create a task whose repo resolves to a different account than you (the calling orchestrator) are running under — this catches spawning into a mismatched/wrong repo. If you deliberately intend to spawn across accounts, set this to true to override. Default false.",
        },
      },
      required: ["repo", "prompt", "agent", "model", "effort"],
    },
    handler: (a) =>
      apiCall("POST", "/api/tasks", {
        repo: a.repo,
        prompt: a.prompt,
        branch: a.branch,
        base: a.base,
        title: a.title,
        model: a.model,
        effort: a.effort,
        agent: a.agent,
        confirm_cross_account: a.confirm_cross_account,
        // Self-identify as the parent so the spawned worktree links into this
        // orchestrator's fleet. Flock injects FLOCK_WORKTREE_ID into every
        // session; absent (e.g. run standalone) → no parent linkage.
        parent_id: process.env.FLOCK_WORKTREE_ID
          ? Number(process.env.FLOCK_WORKTREE_ID)
          : undefined,
      }),
  },
  {
    name: "task_remove",
    description:
      "Remove a worktree — same as the ✕ in Flock's sidebar: kills its session and deletes the checkout (the git branch is kept). ONLY use this when the user explicitly asks you to remove specific worktrees; never on your own initiative or as automatic cleanup. Removal happens immediately, with no dialog, and returns {result: \"removed\"}. Refuses worktrees with uncommitted changes unless force is true — only pass force after telling the user what would be lost and getting an explicit yes. A forced removal of a worktree that really has uncommitted changes also asks the user in Flock to approve: this call then waits up to 2 minutes, and fails if the user declined or nobody answered in time (treated as declined). In that case the worktree stays: don't retry unless the user asks you to. Cannot remove orchestrators.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "number", description: "Worktree id (from task_list)" },
        force: {
          type: "boolean",
          description: "Remove even with uncommitted changes (that work is lost). Only with the user's explicit OK. Default false.",
        },
      },
      required: ["id"],
    },
    handler: (a) =>
      apiCall("POST", `/api/worktrees/${a.id}/remove`, {
        force: a.force === true,
        // Names this orchestrator in the user's confirm dialog (forced, dirty removals).
        requested_by: process.env.FLOCK_WORKTREE_ID
          ? Number(process.env.FLOCK_WORKTREE_ID)
          : undefined,
      }),
  },
  {
    name: "review_open",
    description: "Show a worktree or GitHub PR explained in plain Spanish in Flock’s read-only Revisar tab. Generate/update only when the user asks. Generates a quick independent Sonnet explanation only if the current commit has no cache, reusing unchanged file explanations after new commits. Cached opens cost nothing. Returns status, eval_count and cost_usd. Supply exactly one of worktree_id or pr_url; a URL opens a review-only worktree with no author agent. generate defaults to true; false only opens the cache.",
    inputSchema: { type: "object", properties: { worktree_id: { type: "number" }, pr_url: { type: "string" }, generate: { type: "boolean", default: true } }, oneOf: [{ required: ["worktree_id"] }, { required: ["pr_url"] }] },
    handler: (a) =>
      apiCall("POST", "/api/review/open", {
        worktree_id: a.worktree_id,
        pr_url: a.pr_url,
        generate: a.generate !== false,
        // A URL that creates a review worktree links it into this orchestrator's
        // fleet, like task_create. Absent (run standalone) → no parent.
        parent_id: process.env.FLOCK_WORKTREE_ID
          ? Number(process.env.FLOCK_WORKTREE_ID)
          : undefined,
      }),
  },
  {
    name: "review_get",
    description: "Read the displayed cached review as compact numbered findings: verdict_es, n, path, side, start/end, severity, kind, text_es, fix_es, comment_en, comment_es. No generation. Speak Spanish and refer to evals by number. For the user’s own PR, forward chosen fix_es items to its agent with task_input(submit:true), or make the changes yourself if already in that worktree. Parts/flows are omitted unless include_context is true. Supply exactly one of worktree_id or pr_url.",
    inputSchema: { type: "object", properties: { worktree_id: { type: "number" }, pr_url: { type: "string" }, include_context: { type: "boolean", default: false } }, oneOf: [{ required: ["worktree_id"] }, { required: ["pr_url"] }] },
    handler: (a) => apiCall("POST", "/api/review/get", { worktree_id: a.worktree_id, pr_url: a.pr_url, include_context: a.include_context === true }),
  },
  {
    name: "review_post_preview",
    description: "Validate and freeze an exact GitHub review in English, mapping comments from displayed eval numbers or explicit diff lines. Does NOT post. Translate the user’s Spanish yourself into short, human English; no model call is made here. Show her the EXACT final English body and every comment, with a Spanish gloss under each, and get an EXPLICIT YES before calling review_post(preview_id). A request to prepare, preview, or edit is not approval. Any edit requires a new preview and a new explicit yes. Previews expire after 30 minutes or one posting attempt. Supply exactly one of worktree_id or pr_url.",
    inputSchema: { type: "object", properties: {
      worktree_id: { type: "number" }, pr_url: { type: "string" },
      event: { type: "string", enum: ["COMMENT", "REQUEST_CHANGES", "APPROVE"] }, body_en: { type: "string" },
      comments: { type: "array", items: { type: "object", properties: { n: { type: "integer", minimum: 1 }, path: { type: "string" }, side: { type: "string", enum: ["LEFT", "RIGHT"] }, start: { type: "integer", minimum: 1 }, end: { type: "integer", minimum: 1 }, text_en: { type: "string" } }, required: ["text_en"], oneOf: [{ required: ["n"] }, { required: ["path", "side", "start", "end"] }] } },
    }, required: ["event", "body_en", "comments"], oneOf: [{ required: ["worktree_id"] }, { required: ["pr_url"] }] },
    handler: (a) => apiCall("POST", "/api/review/post-preview", { worktree_id: a.worktree_id, pr_url: a.pr_url, event: a.event, body_en: a.body_en, comments: a.comments }),
  },
  {
    name: "review_post",
    description: "Post EXACTLY the immutable payload stored by review_post_preview. ONLY call after showing the user the exact final English body and every comment with a Spanish gloss under each AND receiving her EXPLICIT YES in chat. Never infer approval from preparing, previewing, or editing a review. Takes only preview_id, so text cannot change after approval. Single-use; expires after 30 minutes. If the PR changed or the preview expired, create a new preview, show the exact text again, and obtain a new explicit yes. If publication was ambiguous, inspect GitHub before trying a new preview.",
    inputSchema: { type: "object", properties: { preview_id: { type: "string" } }, required: ["preview_id"], additionalProperties: false },
    handler: (a) => apiCall("POST", "/api/review/post", { preview_id: a.preview_id }),
  },
  {
    name: "review_deep",
    description: "Start a separate interactive Claude Opus session running /review-pr <pr_url> --flock-json <path>. WARN the user in Spanish that this costs much more than the quick review and obtain an EXPLICIT YES BEFORE calling this tool. Never start it automatically or treat a request for the quick explanation as consent. Flock shows the deep review in the read-only tab when its validated JSON appears. Supply exactly one of worktree_id or pr_url.",
    inputSchema: { type: "object", properties: { worktree_id: { type: "number" }, pr_url: { type: "string" } }, oneOf: [{ required: ["worktree_id"] }, { required: ["pr_url"] }] },
    handler: (a) => apiCall("POST", "/api/review/deep", { worktree_id: a.worktree_id, pr_url: a.pr_url }),
  },
  {
    name: "task_list",
    description: "List all worktrees with their live agent status (working / idle / needs_input).",
    inputSchema: { type: "object", properties: {} },
    handler: () => apiCall("GET", "/api/worktrees"),
  },
  {
    name: "task_status",
    description: "Summary counts of agents by status across all worktrees.",
    inputSchema: { type: "object", properties: {} },
    handler: () => apiCall("GET", "/api/status"),
  },
  {
    name: "task_read",
    description:
      "Read a worktree agent's conversation transcript — the clean message history (user + assistant turns), parsed from the session. A pending AskUserQuestion appears as an assistant message prefixed \"[waiting for answer]\" with its options. Use this to follow what a child agent you spawned is actually doing.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "number", description: "Worktree id (from task_list)" },
        since: {
          type: "number",
          description: "Byte offset already seen; only newer messages are returned (incremental). Omit for the latest slice.",
        },
      },
      required: ["id"],
    },
    handler: (a) =>
      apiCall(
        "GET",
        `/api/worktrees/${a.id}/transcript${a.since ? `?since=${a.since}` : ""}`,
      ),
  },
  {
    name: "task_input",
    description:
      "Send input to a worktree's agent. To send a MESSAGE the agent will act on, pass `text` with `submit: true` — that types it AND presses Enter. Plain `text` (no submit) only types into the composer and leaves it UNSENT, so the agent won't act until something presses Enter. You can also send a special key alone (enter, escape, tab, shift-tab, up, down, left, right, backspace, ctrl-c). NOTE: for an agent's INITIAL task, prefer task_create's `prompt` — it runs automatically as the first turn; use task_input for follow-ups and answering questions.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "number", description: "Worktree id (from task_list)" },
        text: { type: "string", description: "Literal text to type" },
        submit: {
          type: "boolean",
          description: "When sending text, also press Enter to submit it (default false). Use this to actually send a message.",
        },
        key: { type: "string", description: "A special key name (sent on its own)" },
      },
      required: ["id"],
    },
    handler: (a) =>
      apiCall(
        "POST",
        `/api/worktrees/${a.id}/input`,
        a.text != null ? { text: a.text, submit: a.submit === true } : { key: a.key },
      ),
  },
  {
    name: "notify_orchestrator",
    description:
      "Message the Flock orchestrator that spawned you (only works if one did). Flock finds your orchestrator itself — no id needed — and delivers `text` to it tagged with your task. Use it when you finish (kind \"done\": short summary + PR URL if any), are stuck (\"blocked\"), need a decision (\"question\", sent before you stop to ask), or found something it must know now (\"info\"). Keep it short; no progress updates.",
    inputSchema: {
      type: "object",
      properties: {
        kind: { type: "string", enum: ["done", "blocked", "question", "info"] },
        text: { type: "string", description: "The message, a sentence or two" },
      },
      required: ["kind", "text"],
    },
    handler: (a) => {
      if (!process.env.FLOCK_WORKTREE_ID) {
        throw new Error("notify_orchestrator only works inside a Flock session (FLOCK_WORKTREE_ID is unset)");
      }
      return apiCall("POST", "/api/notify", {
        from: Number(process.env.FLOCK_WORKTREE_ID),
        kind: a.kind,
        text: a.text,
      });
    },
  },
  {
    name: "schedule_create",
    description:
      "Create a scheduled task. spec is '@every 30m' / '@every 2h' / '@every 1d' or 'HH:MM' (daily, local time).",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        prompt: { type: "string" },
        spec: { type: "string" },
        title: { type: "string" },
        model: {
          type: "string",
          enum: [
            "opus",
            "sonnet",
            "haiku",
            "fable",
            "claude-opus-5-5",
            "claude-sonnet-5",
            "claude-haiku-4-5-20251001",
            "claude-fable-5-1",
            // Codex agents (agent: "codex"). "default" = Codex's configured model.
            "default",
            "gpt-6.1-sol",
            "gpt-6-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
          ],
          description:
            "REQUIRED. Model applied to every task this schedule fires; must match the agent. Claude agents: an alias or full Claude model id — haiku = mechanical, sonnet = scoped/typical work, opus = ambiguous/high-stakes; fable ONLY if the user asks. Codex agents: \"default\" almost always; a gpt-… id only if the user names it. Always explicit.",
        },
        effort: {
          type: "string",
          enum: ["low", "medium", "high", "xhigh", "max", "default"],
          description: "REQUIRED. Reasoning effort applied to every task this schedule fires. haiku→low; small scoped sonnet→medium; typical multi-file sonnet→high; opus→high, or xhigh for the hardest/highest-stakes; Codex: same difficulty ladder. `max` only if the user asks. If unsure, pick the higher one. \"default\" is accepted for Codex agents only.",
        },
        agent: {
          type: "string",
          enum: ["claude", "codex"],
          description:
            "REQUIRED. \"claude\" or \"codex\" — the agent every fired task runs. Default claude; codex ONLY if the user asks for it, Claude is out of credit, or you are running as Codex. Never switch provider on your own initiative. Codex is only allowed in repos on the Thanx or Flock profile; Flock refuses otherwise.",
        },
        confirm_cross_account: {
          type: "boolean",
          description:
            "Flock refuses to create a schedule whose repo resolves to a different Claude account than you (the calling orchestrator) are running under. If you deliberately intend to spawn across accounts, set this to true to override. Default false.",
        },
      },
      required: ["repo", "prompt", "spec", "agent", "model", "effort"],
    },
    handler: (a) =>
      apiCall("POST", "/api/schedules", {
        repo: a.repo,
        prompt: a.prompt,
        spec: a.spec,
        title: a.title,
        model: a.model,
        effort: a.effort,
        agent: a.agent,
        confirm_cross_account: a.confirm_cross_account,
        // Self-identify as the parent, mirroring task_create, so fired tasks
        // link into this orchestrator's fleet and the cross-account guard has
        // something to check against.
        parent_id: process.env.FLOCK_WORKTREE_ID
          ? Number(process.env.FLOCK_WORKTREE_ID)
          : undefined,
      }),
  },
  {
    name: "schedule_list",
    description: "List all scheduled tasks.",
    inputSchema: { type: "object", properties: {} },
    handler: () => apiCall("GET", "/api/schedules"),
  },
  {
    name: "kb_search",
    description:
      "Search your knowledge base (an Obsidian vault) — your durable memory across sessions. Returns ranked notes with snippets. Call this BEFORE starting work to recall preferences, prior decisions, conventions, and project facts.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", description: "Search terms" },
        limit: { type: "number", description: "Max results (default 20)" },
      },
      required: ["query"],
    },
    handler: (a) =>
      apiCall(
        "GET",
        `/api/kb/search?q=${encodeURIComponent(a.query)}${a.limit ? `&limit=${a.limit}` : ""}`,
      ),
  },
  {
    name: "kb_read",
    description:
      "Read a full note from the knowledge base by its vault-relative path (as returned by kb_search / kb_list).",
    inputSchema: {
      type: "object",
      properties: { path: { type: "string", description: "Vault-relative path, e.g. memory/deploy.md" } },
      required: ["path"],
    },
    handler: (a) => apiCall("GET", `/api/kb/read?path=${encodeURIComponent(a.path)}`),
  },
  {
    name: "kb_list",
    description: "List notes in the knowledge base, optionally filtered by a vault-relative path prefix.",
    inputSchema: {
      type: "object",
      properties: {
        prefix: { type: "string", description: "Optional path prefix, e.g. memory/" },
        limit: { type: "number", description: "Max results (default 100)" },
      },
    },
    handler: (a) =>
      apiCall(
        "GET",
        `/api/kb/list?prefix=${encodeURIComponent(a.prefix ?? "")}${a.limit ? `&limit=${a.limit}` : ""}`,
      ),
  },
  {
    name: "kb_ingest",
    description:
      "Write or update a note in the knowledge base (and the Obsidian vault on disk). Use this to save durable, reusable learnings: user preferences, decisions, project facts, conventions. `path` is vault-relative (e.g. 'memory/deploy-process.md'); `content` is full markdown — include YAML front-matter (title, tags) and [[wiki-links]] to related notes. Search first to avoid duplicates; prefer updating an existing note over creating a near-duplicate.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string", description: "Vault-relative path ending in .md" },
        content: { type: "string", description: "Full markdown content of the note" },
      },
      required: ["path", "content"],
    },
    handler: (a) => apiCall("POST", "/api/kb/ingest", { path: a.path, content: a.content }),
  },
  {
    name: "kb_delete",
    description: "Delete a note from the knowledge base and the vault by its vault-relative path.",
    inputSchema: {
      type: "object",
      properties: { path: { type: "string" } },
      required: ["path"],
    },
    handler: (a) => apiCall("POST", "/api/kb/delete", { path: a.path }),
  },
];

/// Server instructions: tell the agent to treat the knowledge base as durable
/// memory. Mirrors argus's kbInstructions — search-first, write learnings back.
const INSTRUCTIONS = `Flock exposes a persistent Knowledge Base backed by an Obsidian vault — treat it as your durable memory across sessions.

- Before starting a task, call kb_search for relevant context: user preferences, prior decisions, project facts, naming/style conventions.
- When you learn something durable and reusable, call kb_ingest to save it as a markdown note. Use a vault-relative path (e.g. memory/<slug>.md), include YAML front-matter with a title and tags, and link related notes with [[wiki-links]]. Search first to avoid duplicates — update the existing note instead of creating a near-duplicate.
- Use kb_read to open a note by path and kb_list to browse.

Keep notes atomic and concise (one fact or topic per note), like good Obsidian notes — the [[links]] form the knowledge graph.`;

const server = new Server(
  { name: "flock", version: "0.1.0" },
  { capabilities: { tools: {} }, instructions: INSTRUCTIONS },
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: TOOLS.map(({ name, description, inputSchema }) => ({
    name,
    description,
    inputSchema,
  })),
}));

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const tool = TOOLS.find((t) => t.name === req.params.name);
  if (!tool) {
    return { isError: true, content: [{ type: "text", text: `unknown tool ${req.params.name}` }] };
  }
  try {
    const result = await tool.handler(req.params.arguments ?? {});
    return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
  } catch (e) {
    return { isError: true, content: [{ type: "text", text: String(e?.message ?? e) }] };
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
