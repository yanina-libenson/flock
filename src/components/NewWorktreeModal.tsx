import { createSignal, createMemo, Show, onMount } from "solid-js";
import {
  repoAllBranches,
  repoDefaultBranch,
  worktreeCreate,
  worktreesList,
  type Repo,
} from "../lib/ipc";
import { setAppStore, openPane } from "../lib/store";
import { GitBranch, Shuffle, X } from "lucide-solid";
import { PLACES, randomPlace } from "../lib/places";
import { appStore } from "../lib/store";

type Mode = "new" | "existing";

function usedBranchNames(): Set<string> {
  const out = new Set<string>();
  for (const list of Object.values(appStore.worktreesByRepo)) {
    for (const w of list) out.add(w.branch.replace(/^flock\//, ""));
  }
  return out;
}

export function NewWorktreeModal(props: {
  repo: Repo;
  onClose: () => void;
}) {
  const [mode, setMode] = createSignal<Mode>("new");
  const [branchName, setBranchName] = createSignal("");
  const [existingBranch, setExistingBranch] = createSignal("");
  const [baseMode, setBaseMode] = createSignal<"default" | "HEAD" | string>(
    "default",
  );
  const [allBranches, setAllBranches] = createSignal<string[]>([]);
  const [defaultBranch, setDefaultBranch] = createSignal<string | null>(null);

  const takenBranches = createMemo(() => {
    const out = new Set<string>();
    for (const list of Object.values(appStore.worktreesByRepo)) {
      for (const w of list) out.add(w.branch);
    }
    return out;
  });

  const filteredExisting = createMemo(() => {
    const q = existingBranch().toLowerCase();
    const taken = takenBranches();
    return allBranches()
      .filter((b) => !taken.has(b))
      .filter((b) => b.toLowerCase().includes(q))
      .slice(0, 8);
  });

  function shuffleName() {
    setBranchName(randomPlace(usedBranchNames()));
  }

  onMount(() => {
    shuffleName();
    repoAllBranches(props.repo.id)
      .then(setAllBranches)
      .catch((e) => console.error(e));
    repoDefaultBranch(props.repo.id)
      .then(setDefaultBranch)
      .catch(() => setDefaultBranch(null));
    setTimeout(() => {
      const el = document.getElementById("branch-input") as
        | HTMLInputElement
        | null;
      el?.focus();
      el?.select();
    }, 0);
  });

  function onSubmit(e?: Event) {
    e?.preventDefault();
    const repoId = props.repo.id;

    if (mode() === "new") {
      const name = branchName().trim();
      if (!name) return;
      const finalBranch = name.includes("/") ? name : `flock/${name}`;
      const base = baseMode();
      props.onClose();
      worktreeCreate({
        repo_id: repoId,
        branch: finalBranch,
        base,
        title: null,
        new_branch: true,
        path: null,
      })
        .then(async (w) => {
          const wts = await worktreesList(repoId);
          setAppStore("worktreesByRepo", repoId, wts);
          openPane(w.id);
        })
        .catch((err) => {
          alert(`Couldn't create worktree:\n${String(err)}`);
        });
    } else {
      const name = existingBranch().trim();
      if (!name) return;
      props.onClose();
      worktreeCreate({
        repo_id: repoId,
        branch: name,
        base: null,
        title: null,
        new_branch: false,
        path: null,
      })
        .then(async (w) => {
          const wts = await worktreesList(repoId);
          setAppStore("worktreesByRepo", repoId, wts);
          openPane(w.id);
        })
        .catch((err) => {
          alert(`Couldn't open worktree:\n${String(err)}`);
        });
    }
  }

  function focusInput(id: string) {
    setTimeout(() => {
      const el = document.getElementById(id) as HTMLInputElement | null;
      el?.focus();
      el?.select();
    }, 0);
  }

  return (
    <div
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onClose();
      }}
    >
      <form
        class="w-[460px] rounded-xl border border-[var(--color-border-strong)] bg-[var(--color-bg-elevated)] shadow-2xl overflow-hidden"
        onSubmit={onSubmit}
      >
        <div class="flex items-center justify-between px-4 py-3 border-b border-[var(--color-border)]">
          <div class="flex items-center gap-2">
            <GitBranch size={14} class="text-[var(--color-accent)]" />
            <span class="text-[13px] font-semibold text-[var(--color-fg)]">
              New worktree — {props.repo.name}
            </span>
          </div>
          <button
            type="button"
            class="p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-muted)]"
            onClick={props.onClose}
          >
            <X size={14} />
          </button>
        </div>

        {/* Mode tabs */}
        <div class="flex gap-1 px-4 pt-3">
          {(["new", "existing"] as const).map((m) => (
            <button
              type="button"
              class="px-3 py-1.5 text-[12px] font-medium rounded-md transition-colors"
              classList={{
                "bg-[var(--color-bg)] text-[var(--color-fg)] border border-[var(--color-border-strong)]":
                  mode() === m,
                "text-[var(--color-fg-muted)] hover:bg-[var(--color-bg-hover)]":
                  mode() !== m,
              }}
              onClick={() => {
                setMode(m);
                focusInput(m === "new" ? "branch-input" : "existing-input");
              }}
            >
              {m === "new" ? "Create new branch" : "Check out existing"}
            </button>
          ))}
        </div>

        <Show when={mode() === "new"}>
          <div class="p-4 space-y-4">
            <label class="block">
              <span class="block text-[11px] uppercase tracking-wide font-semibold text-[var(--color-fg-muted)] mb-1.5">
                Branch name
              </span>
              <div class="flex items-center rounded-md bg-[var(--color-bg)] border border-[var(--color-border)] focus-within:border-[var(--color-accent)] transition">
                <span class="pl-3 pr-1 text-[12px] font-mono text-[var(--color-fg-dim)]">
                  flock/
                </span>
                <input
                  id="branch-input"
                  type="text"
                  class="flex-1 bg-transparent px-1 py-2 text-[13px] font-mono text-[var(--color-fg)] outline-none"
                  placeholder="my-feature"
                  value={branchName()}
                  onInput={(e) => setBranchName(e.currentTarget.value)}
                />
                <button
                  type="button"
                  class="px-2 py-1.5 mr-1 text-[var(--color-fg-muted)] hover:text-[var(--color-accent)] transition"
                  title="Pick a random place name"
                  onClick={shuffleName}
                >
                  <Shuffle size={13} />
                </button>
              </div>
              <span class="block mt-1 text-[11px] text-[var(--color-fg-dim)]">
                Pre-filled with a random place ({PLACES.length} in rotation). Include "/" to skip the flock/ prefix.
              </span>
            </label>

            <label class="block">
              <span class="block text-[11px] uppercase tracking-wide font-semibold text-[var(--color-fg-muted)] mb-1.5">
                Base
              </span>
              <select
                class="w-full rounded-md bg-[var(--color-bg)] border border-[var(--color-border)] px-3 py-2 text-[13px] font-mono text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
                value={baseMode()}
                onChange={(e) => setBaseMode(e.currentTarget.value)}
              >
                <option value="default">
                  Latest origin/{defaultBranch() ?? "main"} (fetched before creating)
                </option>
                <option value="HEAD">HEAD (current working tree)</option>
                {allBranches().map((b) => (
                  <option value={b}>{b}</option>
                ))}
              </select>
            </label>
          </div>
        </Show>

        <Show when={mode() === "existing"}>
          <div class="p-4 space-y-3">
            <label class="block">
              <span class="block text-[11px] uppercase tracking-wide font-semibold text-[var(--color-fg-muted)] mb-1.5">
                Existing branch
              </span>
              <input
                id="existing-input"
                type="text"
                class="w-full rounded-md bg-[var(--color-bg)] border border-[var(--color-border)] focus:border-[var(--color-accent)] px-3 py-2 text-[13px] font-mono text-[var(--color-fg)] outline-none transition"
                placeholder="DATA-4935"
                value={existingBranch()}
                onInput={(e) => setExistingBranch(e.currentTarget.value)}
                autocomplete="off"
              />
              <span class="block mt-1 text-[11px] text-[var(--color-fg-dim)]">
                Local or remote branch. Remote refs are fetched before checkout.
              </span>
            </label>

            <Show when={existingBranch().trim() && filteredExisting().length > 0}>
              <div class="rounded-md border border-[var(--color-border)] bg-[var(--color-bg)] max-h-48 overflow-y-auto">
                {filteredExisting().map((b) => (
                  <button
                    type="button"
                    class="w-full text-left px-3 py-1.5 text-[12px] font-mono text-[var(--color-fg-muted)] hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-fg)] transition"
                    onClick={() => setExistingBranch(b)}
                  >
                    {b}
                  </button>
                ))}
              </div>
            </Show>
          </div>
        </Show>

        <div class="flex items-center justify-end gap-2 px-4 py-3 bg-[var(--color-bg)]/40 border-t border-[var(--color-border)]">
          <button
            type="button"
            class="px-3 py-1.5 text-[12px] rounded-md text-[var(--color-fg-muted)] hover:bg-[var(--color-bg-hover)] active:bg-[var(--color-bg)]"
            onClick={props.onClose}
          >
            Cancel
          </button>
          <button
            type="submit"
            class="px-4 py-1.5 text-[12px] font-semibold rounded-md bg-[var(--color-accent)] text-black hover:brightness-110 active:brightness-75 disabled:brightness-75 disabled:cursor-not-allowed"
            disabled={
              mode() === "new"
                ? !branchName().trim()
                : !existingBranch().trim()
            }
          >
            {mode() === "new" ? "Create worktree" : "Open worktree"}
          </button>
        </div>
      </form>
    </div>
  );
}
