import { createSignal, Show, onMount, For } from "solid-js";
import {
  orchestratorCreate,
  DEFAULT_PERMISSION_MODE,
  type PermissionMode,
} from "../lib/ipc";
import { appStore, addWorktree, openPane } from "../lib/store";
import { Network, X, ShieldOff } from "lucide-solid";

/// Create a repo-less orchestrator session: a Claude that directs a fleet of
/// agents across the registered repos. Mirrors NewWorktreeModal's styling.
export function NewOrchestratorModal(props: { onClose: () => void }) {
  const [prompt, setPrompt] = createSignal("");
  const [title, setTitle] = createSignal("");
  const [autoApprove, setAutoApprove] = createSignal(true);
  const [submitting, setSubmitting] = createSignal(false);

  const permissionMode = (): PermissionMode =>
    autoApprove() ? DEFAULT_PERMISSION_MODE : "default";

  onMount(() => {
    setTimeout(() => {
      const el = document.getElementById("orch-prompt") as
        | HTMLTextAreaElement
        | null;
      el?.focus();
    }, 0);
  });

  function onSubmit(e?: Event) {
    e?.preventDefault();
    const mission = prompt().trim();
    if (!mission || submitting()) return;
    setSubmitting(true);
    orchestratorCreate({
      prompt: mission,
      title: title().trim() || null,
      permission_mode: permissionMode(),
    })
      .then((w) => {
        addWorktree(w);
        openPane(w.id);
        props.onClose();
      })
      .catch((err) => {
        setSubmitting(false);
        alert(`Couldn't create orchestrator:\n${String(err)}`);
      });
  }

  return (
    <div
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onClose();
      }}
    >
      <form
        class="w-[500px] rounded-xl border border-[var(--color-border-strong)] bg-[var(--color-bg-elevated)] shadow-2xl overflow-hidden"
        onSubmit={onSubmit}
      >
        <div class="flex items-center justify-between px-4 py-3 border-b border-[var(--color-border)]">
          <div class="flex items-center gap-2">
            <Network size={14} class="text-[var(--color-accent)]" />
            <span class="text-[13px] font-semibold text-[var(--color-fg)]">
              New orchestrator
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

        <div class="p-4 space-y-4">
          <p class="text-[11.5px] text-[var(--color-fg-muted)] leading-relaxed">
            An orchestrator runs in its own scratch space and directs a fleet of
            agents across your repos. It can spawn worktrees, watch them, and
            unblock them — you'll see its fleet nested beneath it.
          </p>

          <label class="block">
            <span class="block text-[11px] uppercase tracking-wide font-semibold text-[var(--color-fg-muted)] mb-1.5">
              Mission
            </span>
            <textarea
              id="orch-prompt"
              rows={4}
              class="w-full rounded-md bg-[var(--color-bg)] border border-[var(--color-border)] focus:border-[var(--color-accent)] px-3 py-2 text-[13px] text-[var(--color-fg)] outline-none transition resize-y"
              placeholder="e.g. Add structured logging across thanx-api and thanx-loyalty, then open PRs in each."
              value={prompt()}
              onInput={(e) => setPrompt(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) onSubmit(e);
              }}
            />
            <span class="block mt-1 text-[11px] text-[var(--color-fg-dim)]">
              ⌘↵ to create. The orchestrator breaks this into per-repo tasks.
            </span>
          </label>

          <label class="block">
            <span class="block text-[11px] uppercase tracking-wide font-semibold text-[var(--color-fg-muted)] mb-1.5">
              Title <span class="text-[var(--color-fg-dim)]">(optional)</span>
            </span>
            <input
              type="text"
              class="w-full rounded-md bg-[var(--color-bg)] border border-[var(--color-border)] focus:border-[var(--color-accent)] px-3 py-2 text-[13px] text-[var(--color-fg)] outline-none transition"
              placeholder="Auto-generated from the mission if blank"
              value={title()}
              onInput={(e) => setTitle(e.currentTarget.value)}
            />
          </label>

          <div>
            <span class="block text-[11px] uppercase tracking-wide font-semibold text-[var(--color-fg-muted)] mb-1.5">
              Can spawn into ({appStore.repos.length})
            </span>
            <Show
              when={appStore.repos.length > 0}
              fallback={
                <div class="text-[11.5px] text-[var(--color-warn)]">
                  No repos registered yet. Add a repo first so the orchestrator
                  has somewhere to spawn agents.
                </div>
              }
            >
              <div class="flex flex-wrap gap-1.5">
                <For each={appStore.repos}>
                  {(r) => (
                    <span class="px-2 py-0.5 rounded text-[11px] font-mono bg-[var(--color-bg)] border border-[var(--color-border)] text-[var(--color-fg-muted)]">
                      {r.name}
                    </span>
                  )}
                </For>
              </div>
            </Show>
          </div>
        </div>

        <div class="px-4 pb-3">
          <label class="flex items-start gap-2.5 px-3 py-2 rounded-md bg-[var(--color-bg)]/60 border border-[var(--color-border)] cursor-pointer hover:bg-[var(--color-bg-hover)] transition">
            <input
              type="checkbox"
              class="mt-0.5 accent-[var(--color-accent)]"
              checked={autoApprove()}
              onChange={(e) => setAutoApprove(e.currentTarget.checked)}
            />
            <div class="flex-1">
              <div class="flex items-center gap-1.5 text-[12px] font-medium text-[var(--color-fg)]">
                <ShieldOff size={12} class="text-[var(--color-fg-muted)]" />
                Auto-approve permissions
              </div>
              <div class="mt-0.5 text-[11px] text-[var(--color-fg-dim)] leading-snug">
                Launch with{" "}
                <code class="font-mono text-[10.5px]">
                  --permission-mode bypassPermissions
                </code>{" "}
                so the orchestrator can spawn agents without prompting.
              </div>
            </div>
          </label>
        </div>

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
            disabled={!prompt().trim() || submitting()}
          >
            {submitting() ? "Creating…" : "Create orchestrator"}
          </button>
        </div>
      </form>
    </div>
  );
}
