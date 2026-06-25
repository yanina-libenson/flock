import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---------- Types (mirror Rust) ----------

export interface Repo {
  id: number;
  name: string;
  path: string;
  created_at: number;
}

export interface Worktree {
  id: number;
  repo_id: number;
  branch: string;
  path: string;
  title: string | null;
  created_at: number;
  last_used: number | null;
  permission_mode: PermissionMode;
}

export type PermissionMode =
  | "default"
  | "bypassPermissions"
  | "acceptEdits"
  | "auto"
  | "dontAsk"
  | "plan";

export const DEFAULT_PERMISSION_MODE: PermissionMode = "bypassPermissions";

export interface DirtySummary {
  staged: number;
  unstaged: number;
  untracked: number;
}

export interface CreateWorktreeArgs {
  repo_id: number;
  branch: string;
  base: string | null;
  title: string | null;
  new_branch: boolean;
  path: string | null;
  permission_mode: PermissionMode | null;
}

export interface OpenSessionArgs {
  worktree_id: number;
  cols: number;
  rows: number;
}

// ---------- Repo ----------

export const repoAdd = (path: string) => invoke<Repo>("repo_add", { path });
export const reposList = () => invoke<Repo[]>("repos_list");
export const repoRemove = (id: number) => invoke<void>("repo_remove", { id });
export const repoBranches = (id: number) =>
  invoke<string[]>("repo_branches", { id });
export const repoDefaultBranch = (id: number) =>
  invoke<string>("repo_default_branch", { id });
export const repoAllBranches = (id: number) =>
  invoke<string[]>("repo_all_branches", { id });

// ---------- Worktree ----------

export const worktreeCreate = (args: CreateWorktreeArgs) =>
  invoke<Worktree>("worktree_create", { args });
export const worktreesList = (repoId: number) =>
  invoke<Worktree[]>("worktrees_list", { repoId });
export const worktreeRemove = (id: number, force: boolean) =>
  invoke<void>("worktree_remove", { id, force });
export const worktreeDirty = (id: number) =>
  invoke<DirtySummary>("worktree_dirty", { id });
export const worktreeCurrentBranch = (id: number) =>
  invoke<string>("worktree_current_branch", { id });
export const worktreeSetPermissionMode = (id: number, mode: PermissionMode) =>
  invoke<void>("worktree_set_permission_mode", { id, mode });
export const worktreeSetTitle = (id: number, title: string) =>
  invoke<void>("worktree_set_title", { id, title });
export const worktreeResizeWindow = (id: number, cols: number, rows: number) =>
  invoke<void>("worktree_resize_window", { worktreeId: id, cols, rows });

// ---------- Session / PTY ----------
//
// The "session" is a tmux session owned by the tmux server, named
// `flock-<worktree_id>`. These commands manage the PTY *client* that connects
// xterm in the frontend to the tmux session. Keyed by worktree_id throughout.

export const sessionOpen = (args: OpenSessionArgs) =>
  invoke<void>("session_open", { args });
export const sessionWrite = (worktreeId: number, b64: string) =>
  invoke<void>("session_write", { worktreeId, b64 });
export const sessionResize = (worktreeId: number, cols: number, rows: number) =>
  invoke<void>("session_resize", { worktreeId, cols, rows });
export const sessionClose = (worktreeId: number) =>
  invoke<void>("session_close", { worktreeId });
/// Tell the backend which pane is focused so the idle-hibernation monitor
/// never reaps the session you're looking at. Pass null when no pane is active.
export const setActiveWorktree = (worktreeId: number | null) =>
  invoke<void>("set_active_worktree", { worktreeId });
export const tmuxCheck = () => invoke<boolean>("tmux_check");

// ---------- Remote API / PWA ----------

export interface RemoteInfo {
  running: boolean;
  token: string;
  urls: string[];
}

export const remoteStart = () => invoke<RemoteInfo>("remote_start");
export const remoteStop = () => invoke<RemoteInfo>("remote_stop");
export const remoteInfo = () => invoke<RemoteInfo>("remote_info");

// ---------- Environments (per-folder env vars) ----------

export interface FlockEnvironment {
  name: string;
  vars: Record<string, string>;
}

export interface EnvBinding {
  path: string;
  env: string;
}

export interface EnvConfig {
  environments: FlockEnvironment[];
  bindings: EnvBinding[];
}

export const envConfigGet = () => invoke<EnvConfig>("env_config_get");
export const envConfigSet = (config: EnvConfig) =>
  invoke<void>("env_config_set", { config });

// ---------- Scheduled tasks ----------

export interface Schedule {
  id: number;
  repo_id: number;
  prompt: string;
  spec: string;
  title: string | null;
  enabled: boolean;
  last_run: number | null;
  next_run: number;
  created_at: number;
}

export interface CreateScheduleArgs {
  repo_id: number;
  prompt: string;
  spec: string;
  title: string | null;
}

export const scheduleList = () => invoke<Schedule[]>("schedule_list");
export const scheduleCreate = (args: CreateScheduleArgs) =>
  invoke<Schedule>("schedule_create", { args });
export const scheduleSetEnabled = (id: number, enabled: boolean) =>
  invoke<void>("schedule_set_enabled", { id, enabled });
export const scheduleDelete = (id: number) =>
  invoke<void>("schedule_delete", { id });
export const scheduleRunNow = (id: number) =>
  invoke<Worktree>("schedule_run_now", { id });

// ---------- Knowledge base (Obsidian vault) ----------

export interface KbHit {
  path: string;
  title: string;
  snippet: string;
}

/// The configured vault path, or null if unset.
export const kbGetVault = () => invoke<string | null>("kb_get_vault");
/// Point the KB at a vault folder (created if missing), index it, start
/// watching. Returns the number of notes indexed.
export const kbSetVault = (path: string) =>
  invoke<number>("kb_set_vault", { path });
/// Re-scan the configured vault. Returns the number of notes (re)indexed.
export const kbReindex = () => invoke<number>("kb_reindex");
export const kbSearch = (query: string, limit?: number) =>
  invoke<KbHit[]>("kb_search", { query, limit });

// ---------- Task creation (desktop) ----------

export interface CreateTaskArgs {
  repo_id: number;
  prompt: string;
  branch?: string | null;
  base?: string | null;
  title?: string | null;
  permission_mode?: PermissionMode | null;
}

export const taskCreate = (args: CreateTaskArgs) =>
  invoke<Worktree>("task_create", { args });

// ---------- Events ----------

export interface PtyOutput {
  worktree_id: number;
  b64: string;
}

export interface PtyExit {
  worktree_id: number;
}

/// Agent activity for a worktree, derived by the backend monitor from the
/// session's rendered tmux screen. `needs_input` is the one that pulls you in.
export type WorktreeStatus = "working" | "idle" | "needs_input";

export interface WorktreeStatusEvent {
  worktree_id: number;
  status: WorktreeStatus;
}

export interface WorktreeTitleEvent {
  worktree_id: number;
  title: string;
}

/// The monitor hibernated this worktree's session (idle too long) to free
/// memory. Its tmux session + `claude` are gone; reopening the pane resumes
/// the conversation from disk.
export interface WorktreeHibernatedEvent {
  worktree_id: number;
}

/// What to show for a worktree: its auto-generated title when present, else
/// the branch name (the place slug).
export function worktreeLabel(w: Worktree): string {
  return w.title && w.title.trim() ? w.title.trim() : w.branch;
}

export const onPtyOutput = (cb: (e: PtyOutput) => void): Promise<UnlistenFn> =>
  listen<PtyOutput>("pty:output", (e) => cb(e.payload));

export const onPtyExit = (cb: (e: PtyExit) => void): Promise<UnlistenFn> =>
  listen<PtyExit>("pty:exit", (e) => cb(e.payload));

export const onWorktreeStatus = (
  cb: (e: WorktreeStatusEvent) => void,
): Promise<UnlistenFn> =>
  listen<WorktreeStatusEvent>("worktree:status", (e) => cb(e.payload));

export const onWorktreeTitle = (
  cb: (e: WorktreeTitleEvent) => void,
): Promise<UnlistenFn> =>
  listen<WorktreeTitleEvent>("worktree:title", (e) => cb(e.payload));

export const onWorktreeHibernated = (
  cb: (e: WorktreeHibernatedEvent) => void,
): Promise<UnlistenFn> =>
  listen<WorktreeHibernatedEvent>("worktree:hibernated", (e) => cb(e.payload));
