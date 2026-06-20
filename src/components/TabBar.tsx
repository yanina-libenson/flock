import { For, Show, type Accessor } from "solid-js";
import { GitBranch, X } from "lucide-solid";
import {
  appStore,
  closePane,
  setActivePane,
} from "../lib/store";
import { worktreeLabel, type Worktree } from "../lib/ipc";

/// The tab strip, rendered inside the single-row top bar. iTerm-style: the
/// active tab is a soft filled pill (no border), inactive tabs are dim text,
/// and the close × fades in on hover.
export function TabBar(props: {
  worktreesById: Accessor<Map<number, Worktree>>;
}) {
  return (
    <div class="flex items-center gap-1">
      <For each={appStore.openPaneIds}>
        {(id) => {
          const w = () => props.worktreesById().get(id);
          const active = () => appStore.activePaneId === id;
          return (
            <Show when={w()}>
              <div
                class="no-drag group flex items-center gap-1.5 px-3 h-7 rounded-lg cursor-pointer text-[12px] font-mono whitespace-nowrap transition"
                classList={{
                  "bg-[var(--color-bg-hover)] text-[var(--color-fg)]": active(),
                  "text-[var(--color-fg-dim)] hover:text-[var(--color-fg-muted)] hover:bg-[var(--color-bg-hover)]/40":
                    !active(),
                }}
                onClick={() => setActivePane(id)}
              >
                <GitBranch size={11} class="shrink-0 opacity-60" />
                <span class="max-w-[200px] truncate">{worktreeLabel(w()!)}</span>
                <button
                  class="opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:text-[var(--color-danger)] transition"
                  onClick={(e) => {
                    e.stopPropagation();
                    closePane(id);
                  }}
                  title="Close tab"
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
