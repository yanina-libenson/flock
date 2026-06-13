import { createStore } from "solid-js/store";
import { createEffect } from "solid-js";
import type { Repo, Worktree, WorktreeStatus } from "./ipc";

export interface AppStoreState {
  repos: Repo[];
  worktreesByRepo: Record<number, Worktree[]>;
  openPaneIds: number[];
  activePaneId: number | null;
  /// Live agent status per worktree id, pushed from the backend monitor.
  /// Absent = no live session (never opened, or exited).
  statusByWorktree: Record<number, WorktreeStatus>;
}

const PERSIST_KEY = "flock.panes.v1";

function loadPersisted(): { openPaneIds: number[]; activePaneId: number | null } {
  try {
    const raw = localStorage.getItem(PERSIST_KEY);
    if (!raw) return { openPaneIds: [], activePaneId: null };
    const parsed = JSON.parse(raw);
    return {
      openPaneIds: Array.isArray(parsed.openPaneIds) ? parsed.openPaneIds : [],
      activePaneId:
        typeof parsed.activePaneId === "number" ? parsed.activePaneId : null,
    };
  } catch {
    return { openPaneIds: [], activePaneId: null };
  }
}

const persisted = loadPersisted();

const [store, setStore] = createStore<AppStoreState>({
  repos: [],
  worktreesByRepo: {},
  openPaneIds: persisted.openPaneIds,
  activePaneId: persisted.activePaneId,
  statusByWorktree: {},
});

// Persist on any change to pane state.
createEffect(() => {
  const payload = JSON.stringify({
    openPaneIds: store.openPaneIds,
    activePaneId: store.activePaneId,
  });
  try {
    localStorage.setItem(PERSIST_KEY, payload);
  } catch {
    /* ignore quota errors */
  }
});

export const appStore = store;
export const setAppStore = setStore;

export function openPane(worktreeId: number) {
  setStore((s) => {
    if (s.openPaneIds.includes(worktreeId)) {
      return { ...s, activePaneId: worktreeId };
    }
    return {
      ...s,
      openPaneIds: [...s.openPaneIds, worktreeId],
      activePaneId: worktreeId,
    };
  });
}

export function closePane(worktreeId: number) {
  setStore((s) => {
    const next = s.openPaneIds.filter((id) => id !== worktreeId);
    let active = s.activePaneId;
    if (active === worktreeId) {
      active = next[next.length - 1] ?? null;
    }
    return { ...s, openPaneIds: next, activePaneId: active };
  });
}

export function setActivePane(worktreeId: number | null) {
  setStore("activePaneId", worktreeId);
}

export function setWorktreeStatus(worktreeId: number, status: WorktreeStatus) {
  setStore("statusByWorktree", worktreeId, status);
}

/// Drop a worktree's status (its session exited). Removes the key so the
/// sidebar indicator disappears rather than freezing on the last value.
export function clearWorktreeStatus(worktreeId: number) {
  setStore("statusByWorktree", (prev) => {
    if (!(worktreeId in prev)) return prev;
    const next = { ...prev };
    delete next[worktreeId];
    return next;
  });
}

/// All worktree ids in sidebar order (repos, then worktrees within each).
function orderedWorktreeIds(): number[] {
  const ids: number[] = [];
  for (const r of store.repos) {
    for (const w of store.worktreesByRepo[r.id] ?? []) ids.push(w.id);
  }
  return ids;
}

/// Worktree ids currently waiting on you, in sidebar order. Reactive — reads
/// the store, so callers inside a tracking scope re-run on status changes.
export function worktreesNeedingInput(): number[] {
  return orderedWorktreeIds().filter(
    (id) => store.statusByWorktree[id] === "needs_input",
  );
}

/// Jump to the next agent waiting for input, cycling through them in sidebar
/// order starting after the active pane. Opens the pane if it isn't already.
/// No-op when nothing needs input.
export function jumpToNextNeedingInput() {
  const needing = worktreesNeedingInput();
  if (needing.length === 0) return;
  const cur = store.activePaneId;
  const idx = cur === null ? -1 : needing.indexOf(cur);
  openPane(needing[(idx + 1) % needing.length]);
}

/// Prune pane ids that no longer reference a real worktree.
/// Called after initial repo/worktree load on app boot.
export function prunePanes() {
  setStore((s) => {
    const knownIds = new Set<number>();
    for (const list of Object.values(s.worktreesByRepo)) {
      for (const w of list) knownIds.add(w.id);
    }
    const openPaneIds = s.openPaneIds.filter((id) => knownIds.has(id));
    const activePaneId =
      s.activePaneId !== null && knownIds.has(s.activePaneId)
        ? s.activePaneId
        : (openPaneIds[openPaneIds.length - 1] ?? null);
    return { ...s, openPaneIds, activePaneId };
  });
}
