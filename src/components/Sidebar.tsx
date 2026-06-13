import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  repoAdd,
  reposList,
  repoRemove,
  worktreesList,
  worktreeRemove,
  worktreeDirty,
  worktreeSetPermissionMode,
  worktreeSetTitle,
  worktreeLabel,
  type Repo,
  type Worktree,
  type DirtySummary,
  type PermissionMode,
} from "../lib/ipc";
import {
  appStore,
  setAppStore,
  openPane,
  closePane,
  prunePanes,
  applyWorktreeTitle,
} from "../lib/store";
import {
  FolderGit2,
  Plus,
  GitBranch,
  X,
  FolderPlus,
  FolderOpen,
  Shield,
  ShieldOff,
  Pencil,
} from "lucide-solid";

export function Sidebar(props: { onCreateWorktree: (repo: Repo) => void }) {
  const [expanded, setExpanded] = createSignal<Record<number, boolean>>({});
  const [dirty, setDirty] = createSignal<Record<number, DirtySummary>>({});
  // Worktree id whose title is being edited inline, plus the draft text.
  const [editingId, setEditingId] = createSignal<number | null>(null);
  const [editDraft, setEditDraft] = createSignal("");

  function startEditTitle(w: Worktree) {
    setEditDraft((w.title ?? "").trim());
    setEditingId(w.id);
  }

  /// Persist the edited title. Guarded on editingId so the input's blur (which
  /// also fires on Enter-save and on Escape-cancel) can't double-write or undo
  /// a cancel — Escape clears editingId first, so this becomes a no-op.
  async function saveTitle(w: Worktree) {
    if (editingId() !== w.id) return;
    const next = editDraft().trim();
    setEditingId(null);
    if (next === (w.title ?? "").trim()) return;
    try {
      await worktreeSetTitle(w.id, next);
      applyWorktreeTitle(w.id, next);
    } catch (e) {
      console.error("worktreeSetTitle failed", e);
    }
  }

  onMount(async () => {
    await refresh();
    const timer = setInterval(pollDirty, 10_000);
    onCleanup(() => clearInterval(timer));
  });

  async function refresh() {
    const repos = await reposList();
    setAppStore("repos", repos);
    // Build the worktree map fresh and replace it atomically so keys for
    // removed repos disappear — otherwise `prunePanes` (which iterates
    // Object.values) would keep treating their ids as known and never prune.
    const nextExpanded: Record<number, boolean> = {};
    const nextWorktrees: Record<number, Worktree[]> = {};
    for (const r of repos) {
      nextExpanded[r.id] = true;
      nextWorktrees[r.id] = await worktreesList(r.id);
    }
    setAppStore("worktreesByRepo", nextWorktrees);
    setExpanded(nextExpanded);
    prunePanes();
    pollDirty();
  }

  let polling = false;
  async function pollDirty() {
    // On a repo with many worktrees a single run can exceed the 10s interval.
    // Without this guard, overlapping runs race and an older result can stomp
    // a fresher one via setDirty.
    if (polling) return;
    polling = true;
    try {
      const next: Record<number, DirtySummary> = {};
      for (const list of Object.values(appStore.worktreesByRepo)) {
        for (const w of list) {
          try {
            next[w.id] = await worktreeDirty(w.id);
          } catch {
            // ignore
          }
        }
      }
      setDirty(next);
    } finally {
      polling = false;
    }
  }

  async function onAddRepo() {
    const selected = await openDialog({
      directory: true,
      multiple: false,
      title: "Select a git repository",
    });
    if (!selected) return;
    const path = Array.isArray(selected) ? selected[0] : selected;
    try {
      await repoAdd(path);
      await refresh();
    } catch (e) {
      console.error(e);
      alert(`Couldn't add repo:\n${String(e)}`);
    }
  }

  async function onRemoveRepo(r: Repo) {
    const worktrees = appStore.worktreesByRepo[r.id] ?? [];
    const msg =
      `Remove "${r.name}" from Flock?\n\n` +
      `• Any running Claude sessions for this repo will be killed.\n` +
      `• Worktree directories on disk are KEPT. Re-adding the repo will\n  re-discover them.\n` +
      `• To reclaim disk space, remove each worktree first (× on the branch)` +
      (worktrees.length > 0
        ? `.\n\n${worktrees.length} worktree(s) currently registered.`
        : `.`);
    if (!confirm(msg)) return;

    // Close any panes for this repo's worktrees.
    for (const w of worktrees) {
      closePane(w.id);
    }
    await repoRemove(r.id);
    await refresh();
  }

  async function onTogglePermissionMode(w: Worktree) {
    const next: PermissionMode =
      w.permission_mode === "bypassPermissions" ? "default" : "bypassPermissions";
    const isOpen = appStore.openPaneIds.includes(w.id);
    const verb = next === "bypassPermissions" ? "auto-approve" : "prompt for";
    const msg =
      `Switch "${w.branch}" to ${verb} permissions?\n\n` +
      (isOpen
        ? `The current Claude session in this workspace will be killed; reopening the pane starts a fresh session with the new mode.`
        : `Next time you open this workspace, Claude will start with the new mode.`);
    if (!confirm(msg)) return;
    try {
      await worktreeSetPermissionMode(w.id, next);
      // Tear down the local pane state too so the next open re-runs session_open.
      if (isOpen) closePane(w.id);
      // Optimistically reflect the new mode in the sidebar.
      const list = appStore.worktreesByRepo[w.repo_id] ?? [];
      setAppStore(
        "worktreesByRepo",
        w.repo_id,
        list.map((x) => (x.id === w.id ? { ...x, permission_mode: next } : x)),
      );
    } catch (e) {
      console.error("worktreeSetPermissionMode failed", e);
      alert(`Couldn't change permission mode:\n${String(e)}`);
    }
  }

  async function onRemoveWorktree(w: Worktree) {
    if (
      !confirm(
        `Remove worktree "${w.branch}"?\nThis deletes the worktree directory at\n${w.path}`,
      )
    )
      return;
    try {
      await worktreeRemove(w.id, false);
    } catch {
      if (confirm("Worktree is dirty or locked. Force remove?")) {
        await worktreeRemove(w.id, true);
      } else {
        return;
      }
    }
    closePane(w.id);
    await refresh();
  }

  function toggleExpand(id: number) {
    setExpanded((prev) => ({ ...prev, [id]: !prev[id] }));
  }

  function dirtyDotColor(d?: DirtySummary): string | null {
    if (!d) return null;
    if (d.staged > 0) return "var(--color-warn)";
    if (d.unstaged > 0 || d.untracked > 0) return "var(--color-accent)";
    return null;
  }

  async function openInFinder(path: string) {
    try {
      await revealItemInDir(path);
    } catch (e) {
      console.error(e);
    }
  }

  return (
    <aside class="w-64 shrink-0 border-r border-[var(--color-border)] bg-[var(--color-bg-elevated)]/40 flex flex-col overflow-hidden">
      <div class="flex items-center justify-between px-3 py-2 border-b border-[var(--color-border)]">
        <span class="text-[10px] font-semibold tracking-[0.14em] uppercase text-[var(--color-fg-dim)]">
          Repositories
        </span>
        <button
          class="p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-muted)] hover:text-[var(--color-fg)] transition"
          title="Add repository"
          onClick={onAddRepo}
        >
          <FolderPlus size={14} />
        </button>
      </div>

      <div class="flex-1 overflow-y-auto py-1">
        <Show
          when={appStore.repos.length > 0}
          fallback={
            <div class="px-4 py-8 text-center text-[var(--color-fg-dim)] text-[12px]">
              <FolderGit2 size={24} class="mx-auto mb-3 opacity-40" />
              <div>No repositories yet.</div>
              <button
                class="mt-3 px-3 py-1.5 text-[11px] font-medium rounded-md bg-[var(--color-accent)]/20 text-[var(--color-accent)] hover:bg-[var(--color-accent)]/30 transition"
                onClick={onAddRepo}
              >
                Add repository
              </button>
            </div>
          }
        >
          <For each={appStore.repos}>
            {(r) => (
              <div class="mb-1">
                <div class="group flex items-center gap-1.5 px-2 py-1 mx-1 rounded-md hover:bg-[var(--color-bg-hover)]">
                  <button
                    class="flex-1 flex items-center gap-2 text-left text-[13px] font-medium text-[var(--color-fg)]"
                    onClick={() => toggleExpand(r.id)}
                  >
                    <FolderGit2
                      size={14}
                      class="text-[var(--color-accent)] shrink-0"
                    />
                    <span class="truncate">{r.name}</span>
                  </button>
                  <button
                    class="opacity-0 group-hover:opacity-100 p-1 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-danger)] transition"
                    title="Remove from Flock"
                    onClick={() => onRemoveRepo(r)}
                  >
                    <X size={12} />
                  </button>
                  <button
                    class="p-1 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-muted)] hover:text-[var(--color-accent)] transition"
                    title="New worktree"
                    onClick={() => props.onCreateWorktree(r)}
                  >
                    <Plus size={13} />
                  </button>
                </div>
                <Show when={expanded()[r.id]}>
                  <div class="ml-3 border-l border-[var(--color-border)] pl-1">
                    <For
                      each={appStore.worktreesByRepo[r.id] ?? []}
                      fallback={
                        <div class="px-3 py-1.5 text-[11px] text-[var(--color-fg-dim)]">
                          no worktrees
                        </div>
                      }
                    >
                      {(w) => {
                        const isActive = () => appStore.activePaneId === w.id;
                        const isOpen = () => appStore.openPaneIds.includes(w.id);
                        const dotColor = () => dirtyDotColor(dirty()[w.id]);
                        const status = () => appStore.statusByWorktree[w.id];
                        return (
                          <div
                            class="group flex items-center gap-1.5 px-2 py-1 mx-1 rounded-md cursor-pointer text-[12px] transition"
                            classList={{
                              "bg-[var(--color-accent)]/15 text-[var(--color-fg)]":
                                isActive(),
                              "hover:bg-[var(--color-bg-hover)]": !isActive(),
                              "text-[var(--color-fg-muted)]":
                                !isActive() && status() !== "needs_input",
                              "text-[var(--color-fg)] font-medium":
                                !isActive() && status() === "needs_input",
                            }}
                            onClick={() => openPane(w.id)}
                          >
                            <span class="w-1.5 shrink-0 flex justify-center">
                              <Show when={status()}>
                                <span
                                  class="w-1.5 h-1.5 rounded-full"
                                  classList={{
                                    "bg-[var(--color-warn)] animate-pulse":
                                      status() === "needs_input",
                                    "bg-[var(--color-accent)]":
                                      status() === "working",
                                    "bg-[var(--color-fg-dim)] opacity-50":
                                      status() === "idle",
                                  }}
                                  title={
                                    status() === "needs_input"
                                      ? "Waiting for your input"
                                      : status() === "working"
                                        ? "Working…"
                                        : "Idle"
                                  }
                                />
                              </Show>
                            </span>
                            <GitBranch size={11} class="shrink-0 opacity-70" />
                            <div class="flex flex-col min-w-0 flex-1">
                              <Show
                                when={editingId() === w.id}
                                fallback={
                                  <>
                                    <span class="truncate leading-tight">
                                      {worktreeLabel(w)}
                                    </span>
                                    <Show when={w.title && w.title.trim()}>
                                      <span class="truncate text-[10px] font-mono text-[var(--color-fg-dim)] leading-tight">
                                        {w.branch}
                                      </span>
                                    </Show>
                                  </>
                                }
                              >
                                <input
                                  ref={(el) =>
                                    queueMicrotask(() => {
                                      el.focus();
                                      el.select();
                                    })
                                  }
                                  class="w-full bg-[var(--color-bg)] border border-[var(--color-border-strong)] rounded px-1 py-0.5 text-[12px] text-[var(--color-fg)] outline-none"
                                  value={editDraft()}
                                  placeholder={w.branch}
                                  onClick={(e) => e.stopPropagation()}
                                  onInput={(e) =>
                                    setEditDraft(e.currentTarget.value)
                                  }
                                  onKeyDown={(e) => {
                                    e.stopPropagation();
                                    if (e.key === "Enter") {
                                      e.preventDefault();
                                      saveTitle(w);
                                    } else if (e.key === "Escape") {
                                      e.preventDefault();
                                      setEditingId(null);
                                    }
                                  }}
                                  onBlur={() => saveTitle(w)}
                                />
                              </Show>
                            </div>
                            <Show when={dotColor()}>
                              <span
                                class="w-1.5 h-1.5 rounded-full"
                                style={{ background: dotColor()! }}
                                title="uncommitted changes"
                              />
                            </Show>
                            <Show when={isOpen() && !isActive()}>
                              <span
                                class="w-1 h-1 rounded-full bg-[var(--color-fg-dim)]"
                                title="open in another pane"
                              />
                            </Show>
                            <button
                              class="p-0.5 rounded hover:bg-[var(--color-bg)] transition"
                              classList={{
                                "text-[var(--color-warn)] opacity-90":
                                  w.permission_mode === "bypassPermissions",
                                "text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] opacity-0 group-hover:opacity-100":
                                  w.permission_mode !== "bypassPermissions",
                              }}
                              title={
                                w.permission_mode === "bypassPermissions"
                                  ? "Auto-approving permissions. Click to require prompts."
                                  : "Prompting for permissions. Click to auto-approve."
                              }
                              onClick={(e) => {
                                e.stopPropagation();
                                onTogglePermissionMode(w);
                              }}
                            >
                              {w.permission_mode === "bypassPermissions" ? (
                                <ShieldOff size={11} />
                              ) : (
                                <Shield size={11} />
                              )}
                            </button>
                            <button
                              class="opacity-0 group-hover:opacity-100 p-0.5 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition"
                              title="Rename (edit title)"
                              onClick={(e) => {
                                e.stopPropagation();
                                startEditTitle(w);
                              }}
                            >
                              <Pencil size={11} />
                            </button>
                            <button
                              class="opacity-0 group-hover:opacity-100 p-0.5 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition"
                              title="Reveal in Finder"
                              onClick={(e) => {
                                e.stopPropagation();
                                openInFinder(w.path);
                              }}
                            >
                              <FolderOpen size={11} />
                            </button>
                            <button
                              class="opacity-0 group-hover:opacity-100 p-0.5 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-danger)] transition"
                              title="Remove worktree"
                              onClick={(e) => {
                                e.stopPropagation();
                                onRemoveWorktree(w);
                              }}
                            >
                              <X size={11} />
                            </button>
                          </div>
                        );
                      }}
                    </For>
                  </div>
                </Show>
              </div>
            )}
          </For>
        </Show>
      </div>
    </aside>
  );
}
