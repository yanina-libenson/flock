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
export const tmuxCheck = () => invoke<boolean>("tmux_check");

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

export const onPtyOutput = (cb: (e: PtyOutput) => void): Promise<UnlistenFn> =>
  listen<PtyOutput>("pty:output", (e) => cb(e.payload));

export const onPtyExit = (cb: (e: PtyExit) => void): Promise<UnlistenFn> =>
  listen<PtyExit>("pty:exit", (e) => cb(e.payload));

export const onWorktreeStatus = (
  cb: (e: WorktreeStatusEvent) => void,
): Promise<UnlistenFn> =>
  listen<WorktreeStatusEvent>("worktree:status", (e) => cb(e.payload));
