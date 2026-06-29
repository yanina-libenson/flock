import { For, Show, createSignal, onMount, type JSX } from "solid-js";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  repoAdd,
  reposList,
  repoRemove,
  worktreesList,
  orchestratorsList,
  worktreeRemove,
  worktreeRefreshPrStatus,
  worktreeSetTitle,
  worktreeLabel,
  type Repo,
  type Worktree,
  type PrStatus,
  type WorktreeStatus,
} from "../lib/ipc";
import {
  appStore,
  setAppStore,
  openPane,
  closePane,
  prunePanes,
  applyWorktreeTitle,
  setWorktreePrStatus,
  sidebarMode,
  setSidebarMode,
} from "../lib/store";
import {
  FolderGit2,
  Plus,
  X,
  FolderOpen,
  Pencil,
  Network,
  ChevronRight,
  ChevronDown,
} from "lucide-solid";

export function Sidebar(props: {
  onCreateWorktree: (repo: Repo) => void;
  onCreateOrchestrator: () => void;
}) {
  const [expanded, setExpanded] = createSignal<Record<number, boolean>>({});
  // Per-orchestrator fleet expand state. Default (absent) = expanded.
  const [orchExpanded, setOrchExpanded] = createSignal<Record<number, boolean>>(
    {},
  );
  const isOrchExpanded = (id: number) => orchExpanded()[id] !== false;
  const toggleOrch = (id: number) =>
    setOrchExpanded((p) => ({ ...p, [id]: !(p[id] !== false) }));
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
    // Paint PR badges on launch; the backend poller pushes updates after this.
    void refreshAllPr();
  });

  /// One-shot pass to seed PR statuses so badges show immediately on launch,
  /// without waiting up to a full backend poll interval. Ongoing changes arrive
  /// via the `worktree:pr_status` event listener in App.tsx.
  async function refreshAllPr() {
    for (const list of Object.values(appStore.worktreesByRepo)) {
      for (const w of list) {
        try {
          setWorktreePrStatus(w.id, await worktreeRefreshPrStatus(w.id));
        } catch {
          // ignore
        }
      }
    }
  }

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
    // Orchestrators live in an internal repo hidden from the list above; load
    // them separately into their own section.
    try {
      setAppStore("orchestrators", await orchestratorsList());
    } catch (e) {
      console.error("orchestratorsList failed", e);
    }
    prunePanes();
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

  async function onRemoveWorktree(w: Worktree) {
    if (w.kind === "orchestrator") {
      if (
        !confirm(
          `Remove orchestrator "${worktreeLabel(w)}"?\n\nIts session ends, but the agents it spawned keep running — they just lose their link to it.`,
        )
      )
        return;
      await worktreeRemove(w.id, false);
      closePane(w.id);
      await refresh();
      return;
    }
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

  /// Whose turn it is, as a Tailwind class pair. warn = your move, accent = in
  /// progress / waiting on others, success = ready to merge, dim = no active
  /// review loop. Written literally so the JIT generates them.
  const PILL_TONES = {
    warn: "text-[var(--color-warn)] bg-[var(--color-warn)]/12",
    accent: "text-[var(--color-accent)] bg-[var(--color-accent)]/12",
    success: "text-[var(--color-success)] bg-[var(--color-success)]/12",
    dim: "text-[var(--color-fg-dim)] bg-[var(--color-fg-dim)]/12",
  };

  type Pill = { label: string; tooltip: string; cls: string; pulse?: boolean };

  /// The single per-row status pill. Live agent activity wins (the most
  /// immediate "is the ball in my court" signal); otherwise fall back to the PR
  /// lifecycle. Null = nothing to show.
  function rowPill(agent: WorktreeStatus | undefined, pr?: PrStatus): Pill | null {
    if (agent === "needs_input")
      return {
        label: "Needs you",
        tooltip: "Claude is waiting for your input",
        cls: PILL_TONES.warn,
        pulse: true,
      };
    if (agent === "working")
      return {
        label: "Working",
        tooltip: "Claude is working…",
        cls: PILL_TONES.accent,
      };
    return pr ? prPill(pr) : null;
  }

  /// PR lifecycle status → pill.
  function prPill(s: PrStatus): Pill {
    const map: Record<
      PrStatus["state"],
      { label: string; tooltip: string; tone: keyof typeof PILL_TONES }
    > = {
      ready_to_submit: {
        label: "Push PR",
        tooltip: "Ready to submit — push & open a PR",
        tone: "dim",
      },
      draft: {
        label: "Draft",
        tooltip: "Draft PR — mark ready when done",
        tone: "warn",
      },
      changes_requested: {
        label: "Changes",
        tooltip: "Changes requested — address review feedback",
        tone: "warn",
      },
      ci_failed: {
        label: "CI failed",
        tooltip: "CI failed — fix the failing checks",
        tone: "warn",
      },
      conflicts: {
        label: "Conflicts",
        tooltip: "Merge conflicts — rebase or resolve",
        tone: "warn",
      },
      comments_to_address: {
        label: "Comments",
        tooltip: "Unresolved review comments to address",
        tone: "warn",
      },
      monitoring_ci: {
        label: "CI",
        tooltip: "Monitoring CI — checks running",
        tone: "accent",
      },
      waiting_review: {
        label: "Review",
        tooltip: "Waiting for review",
        tone: "accent",
      },
      ready_to_merge: {
        label: "Merge",
        tooltip: "Approved & mergeable — ready to merge",
        tone: "success",
      },
      merged: { label: "Merged", tooltip: "PR merged", tone: "dim" },
      closed: { label: "Closed", tooltip: "PR closed", tone: "dim" },
    };
    const m = map[s.state];
    return { label: m.label, tooltip: m.tooltip, cls: PILL_TONES[m.tone] };
  }

  async function openInFinder(path: string) {
    try {
      await revealItemInDir(path);
    } catch (e) {
      console.error(e);
    }
  }

  const repoName = (repoId: number) =>
    appStore.repos.find((r) => r.id === repoId)?.name ?? "repo";

  /// The fleet of a given orchestrator: every worktree whose parent_id points
  /// at it, across all repos. Reactive (reads the store).
  const fleetOf = (orchId: number): Worktree[] => {
    const out: Worktree[] = [];
    for (const list of Object.values(appStore.worktreesByRepo)) {
      for (const w of list) if (w.parent_id === orchId) out.push(w);
    }
    return out;
  };

  // Shared row geometry so group rows (repo / orchestrator) and leaf rows
  // (worktree / agent) line up exactly. `mx-1.5` margin + the children wrapper's
  // `ml-[18px]` guide line give one consistent indent step at every level.
  const ROW =
    "group/row relative flex items-center gap-1.5 mx-1 px-2 py-1 rounded-md cursor-pointer transition-colors";
  // Children sit indented well past the parent's icon, with a guide line that
  // drops from under it — so the parent visually "contains" them. The rows keep
  // the full remaining width for their text.
  const CHILDREN_WRAP =
    "ml-[30px] mt-0.5 mb-1 border-l border-[var(--color-border)]";

  /// A small count chip (worktrees in a repo, agents in a fleet).
  const CountChip = (p: { n: number }) => (
    <span class="shrink-0 min-w-[1.25rem] text-center px-1.5 py-0.5 rounded-full text-[10px] font-semibold tabular-nums text-[var(--color-fg-dim)] bg-[var(--color-fg)]/10">
      {p.n}
    </span>
  );

  /// One worktree row — a leaf (worktree / fleet agent) or, with `leading`+`icon`,
  /// an orchestrator group row. `subtitle` overrides the default branch caption
  /// (the fleet passes "repo · branch").
  const WorktreeRow = (rowProps: {
    w: Worktree;
    subtitle?: string;
    leading?: JSX.Element;
    icon?: typeof Network;
    badge?: number;
  }) => {
    const w = rowProps.w;
    const Icon = rowProps.icon;
    const isActive = () => appStore.activePaneId === w.id;
    const status = () => appStore.statusByWorktree[w.id];
    const pill = () => rowPill(status(), appStore.prStatusByWorktree[w.id]);
    const subtitle = () =>
      rowProps.subtitle ??
      (w.kind !== "orchestrator" && w.title && w.title.trim() ? w.branch : null);
    return (
      <div
        class={ROW}
        classList={{
          "bg-[var(--color-accent)]/12 text-[var(--color-fg)] shadow-[inset_2px_0_0_var(--color-accent)]":
            isActive(),
          "hover:bg-[var(--color-bg-hover)]": !isActive(),
          "text-[var(--color-fg-muted)]":
            !isActive() && status() !== "needs_input",
          "text-[var(--color-fg)]": !isActive() && status() === "needs_input",
        }}
        onClick={() => openPane(w.id)}
      >
        {rowProps.leading}
        {Icon && (
          <Icon size={14} class="shrink-0 text-[var(--color-accent)]" />
        )}
        <div class="flex flex-col min-w-0 flex-1 leading-tight">
          <Show
            when={editingId() === w.id}
            fallback={
              <>
                <span
                  class="truncate text-[12.5px] font-medium text-[var(--color-fg)]"
                  title={worktreeLabel(w)}
                >
                  {worktreeLabel(w)}
                </span>
                <Show when={subtitle()}>
                  <span class="truncate text-[10.5px] font-mono text-[var(--color-fg-dim)] mt-px">
                    {subtitle()}
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
              onInput={(e) => setEditDraft(e.currentTarget.value)}
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
        {/* Trailing cluster: count chip, status pill, hover actions. Actions
            replace the chip+pill on hover so a narrow row never crowds. */}
        <Show when={rowProps.badge !== undefined}>
          <span class="group-hover/row:hidden">
            <CountChip n={rowProps.badge!} />
          </span>
        </Show>
        <Show when={pill()}>
          {(p) => (
            <span
              class={`group-hover/row:hidden shrink-0 px-1.5 py-0.5 rounded text-[10px] font-medium leading-none truncate max-w-[72px] ${p().cls}`}
              classList={{ "animate-pulse": p().pulse }}
              title={p().tooltip}
            >
              {p().label}
            </span>
          )}
        </Show>
        <div class="hidden group-hover/row:flex items-center gap-0.5 shrink-0">
          <button
            class="p-1 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition"
            title="Rename (edit title)"
            onClick={(e) => {
              e.stopPropagation();
              startEditTitle(w);
            }}
          >
            <Pencil size={12} />
          </button>
          <button
            class="p-1 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition"
            title="Reveal in Finder"
            onClick={(e) => {
              e.stopPropagation();
              openInFinder(w.path);
            }}
          >
            <FolderOpen size={12} />
          </button>
          <button
            class="p-1 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-danger)] transition"
            title={
              w.kind === "orchestrator" ? "Remove orchestrator" : "Remove worktree"
            }
            onClick={(e) => {
              e.stopPropagation();
              onRemoveWorktree(w);
            }}
          >
            <X size={12} />
          </button>
        </div>
      </div>
    );
  };

  /// Section header: uppercase label, an item count, and an action (+) button.
  const SectionHeader = (hp: {
    label: string;
    count: number;
    actionTitle: string;
    onAction: () => void;
  }) => (
    <div class="flex items-center gap-2 px-3 pt-3 pb-1.5">
      <span class="text-[10px] font-semibold tracking-[0.16em] uppercase text-[var(--color-fg-dim)]">
        {hp.label}
      </span>
      <Show when={hp.count > 0}>
        <span class="text-[10px] font-medium tabular-nums text-[var(--color-fg-dim)]/70">
          {hp.count}
        </span>
      </Show>
      <div class="flex-1" />
      <button
        class="p-1 -mr-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-muted)] hover:text-[var(--color-accent)] transition"
        title={hp.actionTitle}
        onClick={(e) => {
          e.stopPropagation();
          hp.onAction();
        }}
      >
        <Plus size={14} />
      </button>
    </div>
  );

  const Chevron = (p: { open: boolean }) => (
    <span class="shrink-0 text-[var(--color-fg-dim)]">
      <Show when={p.open} fallback={<ChevronRight size={14} />}>
        <ChevronDown size={14} />
      </Show>
    </span>
  );

  const OrchestratorsSection = () => (
    <div>
      <SectionHeader
        label="Orchestrators"
        count={appStore.orchestrators.length}
        actionTitle="New orchestrator"
        onAction={() => props.onCreateOrchestrator()}
      />
      <Show
        when={appStore.orchestrators.length > 0}
        fallback={
          <button
            class="mx-1.5 my-1 flex w-[calc(100%-0.75rem)] items-center gap-2 rounded-md px-2 py-2 text-left text-[11.5px] text-[var(--color-fg-dim)] hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-fg-muted)] transition"
            onClick={() => props.onCreateOrchestrator()}
          >
            <Network size={14} class="shrink-0 opacity-60" />
            <span>Spin up a Claude that orchestrates many repos.</span>
          </button>
        }
      >
        <For each={appStore.orchestrators}>
          {(o) => {
            const fleet = () => fleetOf(o.id);
            return (
              <div>
                <WorktreeRow
                  w={o}
                  subtitle=""
                  icon={Network}
                  badge={fleet().length}
                  leading={
                    <button
                      class="-ml-1 shrink-0 rounded p-0.5 text-[var(--color-fg-dim)] hover:bg-[var(--color-bg)] hover:text-[var(--color-fg)] transition"
                      title={isOrchExpanded(o.id) ? "Collapse fleet" : "Expand fleet"}
                      onClick={(e) => {
                        e.stopPropagation();
                        toggleOrch(o.id);
                      }}
                    >
                      <Chevron open={isOrchExpanded(o.id)} />
                    </button>
                  }
                />
                <Show when={isOrchExpanded(o.id)}>
                  <div class={CHILDREN_WRAP}>
                    <For
                      each={fleet()}
                      fallback={
                        <div class="px-3 py-1.5 text-[11px] italic text-[var(--color-fg-dim)]">
                          no agents yet
                        </div>
                      }
                    >
                      {(c) => (
                        <WorktreeRow
                          w={c}
                          subtitle={`${repoName(c.repo_id)} · ${c.branch}`}
                        />
                      )}
                    </For>
                  </div>
                </Show>
              </div>
            );
          }}
        </For>
      </Show>
    </div>
  );

  const ReposSection = () => (
    <div>
      <SectionHeader
        label="Repositories"
        count={appStore.repos.length}
        actionTitle="Add repository"
        onAction={onAddRepo}
      />
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
          {(r) => {
            const list = () => appStore.worktreesByRepo[r.id] ?? [];
            return (
              <div>
                <div
                  class={`${ROW} text-[var(--color-fg)] hover:bg-[var(--color-bg-hover)]`}
                  onClick={() => toggleExpand(r.id)}
                >
                  <Chevron open={!!expanded()[r.id]} />
                  <FolderGit2
                    size={14}
                    class="shrink-0 text-[var(--color-accent)]"
                  />
                  <span class="flex-1 truncate text-[12.5px] font-medium">
                    {r.name}
                  </span>
                  <Show when={list().length > 0}>
                    <span class="group-hover/row:hidden">
                      <CountChip n={list().length} />
                    </span>
                  </Show>
                  <div class="hidden group-hover/row:flex items-center gap-0.5 shrink-0">
                    <button
                      class="p-1 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-muted)] hover:text-[var(--color-accent)] transition"
                      title="New worktree"
                      onClick={(e) => {
                        e.stopPropagation();
                        props.onCreateWorktree(r);
                      }}
                    >
                      <Plus size={13} />
                    </button>
                    <button
                      class="p-1 rounded hover:bg-[var(--color-bg)] text-[var(--color-fg-dim)] hover:text-[var(--color-danger)] transition"
                      title="Remove from Flock"
                      onClick={(e) => {
                        e.stopPropagation();
                        onRemoveRepo(r);
                      }}
                    >
                      <X size={12} />
                    </button>
                  </div>
                </div>
                <Show when={expanded()[r.id]}>
                  <div class={CHILDREN_WRAP}>
                    <For
                      each={list()}
                      fallback={
                        <div class="px-3 py-1.5 text-[11px] italic text-[var(--color-fg-dim)]">
                          no worktrees
                        </div>
                      }
                    >
                      {(w) => <WorktreeRow w={w} />}
                    </For>
                  </div>
                </Show>
              </div>
            );
          }}
        </For>
      </Show>
    </div>
  );

  /// One segment of the view toggle.
  const ModeButton = (mp: {
    mode: "orchestrators" | "repos";
    label: string;
    icon: typeof Network;
  }) => {
    const Icon = mp.icon;
    return (
      <button
        class="flex-1 flex items-center justify-center gap-1.5 px-2 py-1 rounded-md text-[11px] font-medium transition"
        classList={{
          "bg-[var(--color-bg)] text-[var(--color-fg)] shadow-sm": sidebarMode() === mp.mode,
          "text-[var(--color-fg-dim)] hover:text-[var(--color-fg-muted)]":
            sidebarMode() !== mp.mode,
        }}
        title={`Show ${mp.label.toLowerCase()}`}
        onClick={() => setSidebarMode(mp.mode)}
      >
        <Icon size={12} class="shrink-0" />
        {mp.label}
      </button>
    );
  };

  return (
    <aside class="w-64 shrink-0 border-r border-[var(--color-border)] bg-[var(--color-bg-elevated)] flex flex-col overflow-hidden">
      {/* View toggle: orchestrators or repos. The two views are exclusive. */}
      <div class="p-2 border-b border-[var(--color-border)]">
        <div class="flex gap-1 p-0.5 rounded-lg bg-[var(--color-bg)]/60">
          <ModeButton mode="orchestrators" label="Orchestrators" icon={Network} />
          <ModeButton mode="repos" label="Repos" icon={FolderGit2} />
        </div>
      </div>

      <div class="flex-1 overflow-y-auto pb-2">
        <Show when={sidebarMode() === "orchestrators"} fallback={<ReposSection />}>
          <OrchestratorsSection />
        </Show>
      </div>
    </aside>
  );
}
