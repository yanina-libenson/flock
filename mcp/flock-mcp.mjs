#!/usr/bin/env node
// Flock MCP server — lets an agent drive Flock the way you do.
//
// It's a thin stdio bridge to Flock's REST API (the same surface the PWA uses),
// so there's no orchestration logic duplicated here. Add it to an agent with:
//
//   claude mcp add flock -- node /absolute/path/to/flock-mcp.mjs
//
// Requires Flock's "Remote access" toggle to be ON (it starts the API server).
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
      "Spawn a new agent task: create a git worktree in the given repo and start Claude on it with an initial prompt. Returns the new worktree.",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string", description: "Repo name as registered in Flock" },
        prompt: { type: "string", description: "Initial prompt for the agent" },
        branch: { type: "string", description: "Optional branch leaf; auto-derived from the prompt if omitted" },
        base: { type: "string", description: "Optional base branch" },
        title: { type: "string", description: "Optional display title" },
      },
      required: ["repo", "prompt"],
    },
    handler: (a) =>
      apiCall("POST", "/api/tasks", {
        repo: a.repo,
        prompt: a.prompt,
        branch: a.branch,
        base: a.base,
        title: a.title,
      }),
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
    name: "task_input",
    description:
      "Send input to a worktree's agent: literal text, or a special key (enter, escape, tab, shift-tab, up, down, left, right, backspace, ctrl-c).",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "number", description: "Worktree id (from task_list)" },
        text: { type: "string", description: "Literal text to type" },
        key: { type: "string", description: "A special key name" },
      },
      required: ["id"],
    },
    handler: (a) =>
      apiCall("POST", `/api/worktrees/${a.id}/input`, a.text != null ? { text: a.text } : { key: a.key }),
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
      },
      required: ["repo", "prompt", "spec"],
    },
    handler: (a) =>
      apiCall("POST", "/api/schedules", {
        repo: a.repo,
        prompt: a.prompt,
        spec: a.spec,
        title: a.title,
      }),
  },
  {
    name: "schedule_list",
    description: "List all scheduled tasks.",
    inputSchema: { type: "object", properties: {} },
    handler: () => apiCall("GET", "/api/schedules"),
  },
];

const server = new Server(
  { name: "flock", version: "0.1.0" },
  { capabilities: { tools: {} } },
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
