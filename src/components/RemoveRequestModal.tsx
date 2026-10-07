import { createEffect, onCleanup, onMount, Show } from "solid-js";
import { Trash2 } from "lucide-solid";
import { worktreeRemoveAnswer, type RemoveRequest } from "../lib/ipc";
import { appStore, dropRemoveRequest } from "../lib/store";

/// Asks the user to approve an orchestrator's task_remove. Shows the oldest
/// pending request; "Keep" is the default (focused, and Escape picks it). The
/// backend treats no answer within `timeout_secs` as "keep" and closes the
/// dialog itself (`worktree:remove_request_done`).
export function RemoveRequestModal() {
  const req = (): RemoveRequest | undefined => appStore.removeRequests[0];
  let keepRef: HTMLButtonElement | undefined;

  const answer = (approve: boolean) => {
    const r = req();
    if (!r) return;
    dropRemoveRequest(r.request_id);
    worktreeRemoveAnswer(r.request_id, approve).catch((e) =>
      console.error("worktreeRemoveAnswer failed", e),
    );
  };

  // Focus "Keep" for each new request, so a stray Enter typed at the terminal
  // keeps the worktree.
  createEffect(() => {
    if (req()) queueMicrotask(() => keepRef?.focus());
  });

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && req()) {
        e.preventDefault();
        answer(false);
      }
    };
    window.addEventListener("keydown", onKey, true);
    onCleanup(() => window.removeEventListener("keydown", onKey, true));
  });

  return (
    <Show when={req()}>
      {(r) => (
        <div class="fixed inset-0 z-[90] flex items-center justify-center bg-black/50 backdrop-blur-sm">
          <div class="w-[460px] rounded-xl border border-[var(--color-border-strong)] bg-[var(--color-bg-elevated)] shadow-2xl overflow-hidden">
            <div class="flex items-center gap-2 px-4 py-3 border-b border-[var(--color-border)]">
              <Trash2 size={14} class="text-[var(--color-danger)]" />
              <span class="text-[13px] font-semibold text-[var(--color-fg)]">
                Remove worktree?
              </span>
            </div>
            <div class="px-4 py-3 text-[12.5px] text-[var(--color-fg-muted)] leading-relaxed space-y-2">
              <p>
                <span class="text-[var(--color-fg)]">
                  {r().requested_by
                    ? `Orchestrator "${r().requested_by}"`
                    : "An orchestrator"}
                </span>{" "}
                wants to remove this worktree. Its session ends and the checkout
                is deleted; the git branch is kept.
              </p>
              <div class="rounded-md border border-[var(--color-border)] bg-[var(--color-bg)]/60 px-3 py-2">
                <div class="text-[var(--color-fg)] font-medium truncate" title={r().label}>
                  {r().label}
                </div>
                <div class="font-mono text-[11px] text-[var(--color-fg-dim)] truncate">
                  {r().repo} · {r().branch}
                </div>
              </div>
              <Show
                when={r().dirty}
                fallback={<p>No uncommitted changes.</p>}
              >
                {(d) => (
                  <p class="text-[var(--color-danger)]">
                    It has uncommitted changes ({d().staged} staged,{" "}
                    {d().unstaged} unstaged, {d().untracked} untracked). They
                    will be lost.
                  </p>
                )}
              </Show>
              <p class="text-[11px] text-[var(--color-fg-dim)]">
                No answer within {Math.round(r().timeout_secs / 60)} min keeps it.
                <Show when={appStore.removeRequests.length > 1}>
                  {" "}
                  {appStore.removeRequests.length - 1} more request
                  {appStore.removeRequests.length > 2 ? "s" : ""} waiting.
                </Show>
              </p>
            </div>
            <div class="flex items-center justify-end gap-2 px-4 py-3 bg-[var(--color-bg)]/40 border-t border-[var(--color-border)]">
              <button
                ref={keepRef}
                type="button"
                class="px-3 py-1.5 text-[12px] rounded-md text-[var(--color-fg)] bg-[var(--color-bg-hover)] hover:brightness-125"
                onClick={() => answer(false)}
              >
                Keep
              </button>
              <button
                type="button"
                class="px-4 py-1.5 text-[12px] font-semibold rounded-md bg-[var(--color-danger)] text-black hover:brightness-110 active:brightness-75"
                onClick={() => answer(true)}
              >
                Remove
              </button>
            </div>
          </div>
        </div>
      )}
    </Show>
  );
}
