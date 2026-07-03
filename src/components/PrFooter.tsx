import { For, Show, createEffect, createMemo } from "solid-js";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  worktreeListPrs,
  worktreeLabel,
  type PrRef,
  type Worktree,
} from "../lib/ipc";
import { appStore, setWorktreePrs } from "../lib/store";
import { GitPullRequest } from "lucide-solid";

/// State pill for a PR, keyed off gh's raw state + draft flag. Tones mirror the
/// sidebar's PR vocabulary so the two surfaces read as one system.
const STATE_TONE = {
  open: "text-[var(--color-accent)] bg-[var(--color-accent)]/12",
  draft: "text-[var(--color-warn)] bg-[var(--color-warn)]/12",
  merged: "text-[var(--color-success)] bg-[var(--color-success)]/12",
  closed: "text-[var(--color-fg-dim)] bg-[var(--color-fg-dim)]/12",
};

function stateLabel(pr: PrRef): { label: string; cls: string } {
  if (pr.state === "MERGED") return { label: "Merged", cls: STATE_TONE.merged };
  if (pr.state === "CLOSED") return { label: "Closed", cls: STATE_TONE.closed };
  if (pr.is_draft) return { label: "Draft", cls: STATE_TONE.draft };
  return { label: "Open", cls: STATE_TONE.open };
}

/// The persistent PR list shown at the foot of the active pane. For a plain
/// worktree it lists that worktree's PRs; for an orchestrator it aggregates the
/// PRs of its whole fleet. Deliberately absent (zero height) when there are no
/// PRs — "always visible" means existing PRs never vanish, not that we reserve
/// space for emptiness.
export function PrFooter(props: { worktree: Worktree }) {
  // Whose PRs to show: an orchestrator aggregates its fleet (children by
  // parent_id, across every repo); a worktree shows only its own. Reactive so a
  // freshly-spawned fleet agent joins the list live.
  const sources = createMemo<Worktree[]>(() => {
    const w = props.worktree;
    if (w.kind !== "orchestrator") return [w];
    const fleet: Worktree[] = [];
    for (const list of Object.values(appStore.worktreesByRepo)) {
      for (const c of list) if (c.parent_id === w.id) fleet.push(c);
    }
    return fleet;
  });

  // Fetch/refresh every source's PRs in parallel. Re-runs when the set of
  // sources changes and when any source's PR lifecycle status flips (the
  // poller's signal that GitHub changed) — no extra timer of our own.
  createEffect(() => {
    const ids = sources().map((w) => w.id);
    // Touch each source's status so a poller update re-triggers this effect.
    for (const id of ids) void appStore.prStatusByWorktree[id];
    void Promise.all(
      ids.map((id) =>
        worktreeListPrs(id)
          .then((prs) => setWorktreePrs(id, prs))
          .catch(() => {}),
      ),
    );
  });

  const isOrchestrator = () => props.worktree.kind === "orchestrator";

  // Flattened, de-duplicated rows across all sources. Dedupe by url (the PR's
  // stable identity) so a re-parented agent can't surface the same PR twice.
  type Row = { pr: PrRef; source: Worktree };
  const rows = createMemo<Row[]>(() => {
    const seen = new Set<string>();
    const out: Row[] = [];
    for (const w of sources()) {
      for (const pr of appStore.prsByWorktree[w.id] ?? []) {
        if (seen.has(pr.url)) continue;
        seen.add(pr.url);
        out.push({ pr, source: w });
      }
    }
    return out;
  });

  return (
    <Show when={rows().length > 0}>
      <div class="shrink-0 border-t border-[var(--color-border)] bg-[var(--color-bg-elevated)] max-h-[30vh] overflow-y-auto">
        <div class="sticky top-0 flex items-center gap-2 px-3 pt-1.5 pb-1 bg-[var(--color-bg-elevated)]">
          <GitPullRequest size={12} class="shrink-0 text-[var(--color-fg-dim)]" />
          <span class="text-[10px] font-semibold tracking-[0.14em] uppercase text-[var(--color-fg-dim)]">
            Pull requests
          </span>
          <span class="text-[10px] font-medium tabular-nums text-[var(--color-fg-dim)]/70">
            {rows().length}
          </span>
        </div>
        <div class="pb-1.5">
          <For each={rows()}>
            {(row) => {
              const st = stateLabel(row.pr);
              return (
                <button
                  class="group/pr flex w-full items-center gap-2 px-3 py-1 text-left hover:bg-[var(--color-bg-hover)] transition-colors"
                  title={`${row.pr.title} — open on GitHub`}
                  onClick={() => openUrl(row.pr.url).catch(() => {})}
                >
                  <span class="shrink-0 text-[11px] font-mono tabular-nums text-[var(--color-fg-dim)]">
                    #{row.pr.number}
                  </span>
                  <span class="flex-1 truncate text-[12px] text-[var(--color-fg-muted)] group-hover/pr:text-[var(--color-fg)]">
                    {row.pr.title}
                  </span>
                  <Show when={isOrchestrator()}>
                    <span class="shrink-0 max-w-[120px] truncate text-[10.5px] font-mono text-[var(--color-fg-dim)]">
                      {worktreeLabel(row.source)}
                    </span>
                  </Show>
                  <span
                    class={`shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium leading-none ${st.cls}`}
                  >
                    {st.label}
                  </span>
                </button>
              );
            }}
          </For>
        </div>
      </div>
    </Show>
  );
}
