import { For, Show, type Accessor } from "solid-js";
import { GitBranch, X } from "lucide-solid";
import {
  appStore,
  closePane,
  setActivePane,
} from "../lib/store";
import type { Worktree } from "../lib/ipc";

export function TabBar(props: {
  worktreesById: Accessor<Map<number, Worktree>>;
}) {
  return (
    <div class="flex items-center h-9 px-2 shrink-0 border-b border-[var(--color-border)] bg-[var(--color-bg-elevated)]/50 overflow-x-auto">
      <For each={appStore.openPaneIds}>
        {(id) => {
          const w = () => props.worktreesById().get(id);
          const active = () => appStore.activePaneId === id;
          return (
            <Show when={w()}>
              <div
                class="group flex items-center gap-1.5 px-3 h-7 rounded-md cursor-pointer mr-1 text-[12px] font-mono transition"
                classList={{
                  "bg-[var(--color-bg)] text-[var(--color-fg)] border border-[var(--color-border-strong)]":
                    active(),
                  "text-[var(--color-fg-muted)] hover:bg-[var(--color-bg-hover)]":
                    !active(),
                }}
                onClick={() => setActivePane(id)}
              >
                <GitBranch size={11} class="shrink-0 opacity-70" />
                <span class="max-w-[240px] truncate">{w()!.branch}</span>
                <button
                  class="opacity-40 hover:opacity-100 hover:text-[var(--color-danger)] transition"
                  onClick={(e) => {
                    e.stopPropagation();
                    closePane(id);
                  }}
                  title="Close pane"
                >
                  <X size={11} />
                </button>
              </div>
            </Show>
          );
        }}
      </For>
    </div>
  );
}
