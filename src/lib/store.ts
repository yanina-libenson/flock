import { createStore } from "solid-js/store";
import { createEffect, createSignal } from "solid-js";
import type { PrStatus, Repo, Worktree, WorktreeStatus } from "./ipc";

export interface AppStoreState {
  repos: Repo[];
  worktreesByRepo: Record<number, Worktree[]>;
  /// Orchestrator sessions (kind='orchestrator'), shown in their own sidebar
  /// section. Their spawned children live in worktreesByRepo under their real
  /// repo; the fleet view filters those by parent_id.
  orchestrators: Worktree[];
  openPaneIds: number[];
  activePaneId: number | null;
  /// Subset of openPaneIds that have been activated this session — the only
  /// panes we mount a TerminalPane (and thus attach claude) for. Lazy attach:
  /// on launch only the restored active pane is here, so reopening Flock
  /// doesn't respawn every worktree's claude at once. A hibernated pane drops
  /// out of this set (back to a dormant tab) until re-activated. NOT persisted
  /// — reset each launch so lazy attach re-applies.
  activatedPaneIds: number[];
  /// Live agent status per worktree id, pushed from the backend monitor.
  /// Absent = no live session (never opened, or exited).
  statusByWorktree: Record<number, WorktreeStatus>;
  /// One-shot notice per worktree explaining a memory-driven hibernation, shown
  /// as a banner when the user reopens the reaped pane. Set on the "memory"
  /// hibernated event, cleared when the user dismisses the banner.
  hibernationNoteByWorktree: Record<number, string>;
  /// PR lifecycle status per worktree id, from the backend PR poller. Absent =
  /// no PR and nothing to submit (no badge shown).
  prStatusByWorktree: Record<number, PrStatus>;
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
  orchestrators: [],
  openPaneIds: persisted.openPaneIds,
  activePaneId: persisted.activePaneId,
  // Seed only the restored active pane: every other open tab stays dormant
  // until clicked, so launch attaches one claude, not all of them.
  activatedPaneIds:
    persisted.activePaneId !== null ? [persisted.activePaneId] : [],
  statusByWorktree: {},
  hibernationNoteByWorktree: {},
  prStatusByWorktree: {},
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

/// Whether the repositories sidebar is shown. Hiding it makes the desktop
/// terminal fill the window like a plain iTerm tab. Persisted across launches.
const SIDEBAR_KEY = "flock.sidebar.visible.v1";
const [sidebarVisible, setSidebarVisibleSig] = createSignal(
  (() => {
    try {
      return localStorage.getItem(SIDEBAR_KEY) !== "0";
    } catch {
      return true;
    }
  })(),
);
export { sidebarVisible };
export function toggleSidebar() {
  setSidebarVisibleSig((v) => {
    const next = !v;
    try {
      localStorage.setItem(SIDEBAR_KEY, next ? "1" : "0");
    } catch {
      /* ignore quota errors */
    }
    return next;
  });
}

/// Which list leads the sidebar: orchestrators (default) or repos. Both lists
/// are always shown — this controls which one is on top (the primary). Lets you
/// flip between an orchestrator-first and a repo-first layout. Persisted.
export type SidebarMode = "orchestrators" | "repos";
const SIDEBAR_MODE_KEY = "flock.sidebar.mode.v1";
const [sidebarMode, setSidebarModeSig] = createSignal<SidebarMode>(
  (() => {
    try {
      return localStorage.getItem(SIDEBAR_MODE_KEY) === "repos"
        ? "repos"
        : "orchestrators";
    } catch {
      return "orchestrators";
    }
  })(),
);
export { sidebarMode };
export function setSidebarMode(mode: SidebarMode) {
  setSidebarModeSig(mode);
  try {
    localStorage.setItem(SIDEBAR_MODE_KEY, mode);
  } catch {
    /* ignore quota errors */
  }
}

/// Add a pane to the activated set (mount it / attach claude) if not already in.
function withActivated(ids: number[], worktreeId: number): number[] {
  return ids.includes(worktreeId) ? ids : [...ids, worktreeId];
}

export function openPane(worktreeId: number) {
  setStore((s) => {
    const activatedPaneIds = withActivated(s.activatedPaneIds, worktreeId);
    if (s.openPaneIds.includes(worktreeId)) {
      return { ...s, activePaneId: worktreeId, activatedPaneIds };
    }
    return {
      ...s,
      openPaneIds: [...s.openPaneIds, worktreeId],
      activePaneId: worktreeId,
      activatedPaneIds,
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
    return {
      ...s,
      openPaneIds: next,
      activePaneId: active,
      activatedPaneIds: s.activatedPaneIds.filter((id) => id !== worktreeId),
    };
  });
}

export function setActivePane(worktreeId: number | null) {
  setStore((s) => ({
    ...s,
    activePaneId: worktreeId,
    activatedPaneIds:
      worktreeId === null
        ? s.activatedPaneIds
        : withActivated(s.activatedPaneIds, worktreeId),
  }));
}

/// The backend monitor hibernated this worktree's session to free memory.
/// Drop it to a dormant tab: unmount its pane (frees the xterm) and clear its
/// status dot. The tab stays in `openPaneIds`; clicking it re-activates →
/// remounts → reattaches with `claude --resume`, restoring the conversation.
/// `note`, when present (the "memory" reason), is stashed so the reopened pane
/// shows a banner explaining the kill.
export function hibernatePane(worktreeId: number, note?: string) {
  setStore("activatedPaneIds", (ids) =>
    ids.filter((id) => id !== worktreeId),
  );
  clearWorktreeStatus(worktreeId);
  if (note) setStore("hibernationNoteByWorktree", worktreeId, note);
}

/// Dismiss the hibernation banner for a worktree (user acknowledged it).
export function clearHibernationNote(worktreeId: number) {
  setStore("hibernationNoteByWorktree", (prev) => {
    if (!(worktreeId in prev)) return prev;
    const next = { ...prev };
    delete next[worktreeId];
    return next;
  });
}

/// Add a worktree pushed from the backend (worktree:created) into the store,
/// live. Orchestrators land in their own list; everything else lands under its
/// repo (so a spawned child appears under its repo and, via parent_id, in the
/// spawning orchestrator's fleet). Deduped by id so a desktop-initiated create
/// that also emits the event doesn't double-insert.
export function addWorktree(w: Worktree) {
  if (w.kind === "orchestrator") {
    setStore("orchestrators", (prev) =>
      prev.some((o) => o.id === w.id) ? prev : [...prev, w],
    );
    return;
  }
  setStore("worktreesByRepo", w.repo_id, (prev) => {
    const list = prev ?? [];
    return list.some((x) => x.id === w.id) ? list : [...list, w];
  });
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

/// Apply a PR lifecycle status pushed from the backend poller. A null status
/// clears the badge (no PR / nothing to submit).
export function setWorktreePrStatus(
  worktreeId: number,
  status: PrStatus | null,
) {
  if (status === null) {
    setStore("prStatusByWorktree", (prev) => {
      if (!(worktreeId in prev)) return prev;
      const next = { ...prev };
      delete next[worktreeId];
      return next;
    });
  } else {
    setStore("prStatusByWorktree", worktreeId, status);
  }
}

/// Apply a title pushed from the backend monitor to the matching worktree,
/// wherever it lives in the repo map.
export function applyWorktreeTitle(worktreeId: number, title: string) {
  for (const repoId of Object.keys(store.worktreesByRepo)) {
    const rid = Number(repoId);
    const idx = (store.worktreesByRepo[rid] ?? []).findIndex(
      (w) => w.id === worktreeId,
    );
    if (idx >= 0) {
      setStore("worktreesByRepo", rid, idx, "title", title);
      return;
    }
  }
  const oIdx = store.orchestrators.findIndex((w) => w.id === worktreeId);
  if (oIdx >= 0) setStore("orchestrators", oIdx, "title", title);
}

/// All worktree ids in sidebar order (orchestrators first, then repos and their
/// worktrees). Used for ⌘J cycling through agents that need you.
function orderedWorktreeIds(): number[] {
  const ids: number[] = [];
  for (const o of store.orchestrators) ids.push(o.id);
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
    for (const o of s.orchestrators) knownIds.add(o.id);
    const openPaneIds = s.openPaneIds.filter((id) => knownIds.has(id));
    const activePaneId =
      s.activePaneId !== null && knownIds.has(s.activePaneId)
        ? s.activePaneId
        : (openPaneIds[openPaneIds.length - 1] ?? null);
    let activatedPaneIds = s.activatedPaneIds.filter((id) =>
      openPaneIds.includes(id),
    );
    // The (possibly newly-chosen) active pane must always be mounted.
    if (activePaneId !== null && !activatedPaneIds.includes(activePaneId)) {
      activatedPaneIds = [...activatedPaneIds, activePaneId];
    }
    return { ...s, openPaneIds, activePaneId, activatedPaneIds };
  });
}
