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
