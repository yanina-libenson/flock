import { Show, createEffect, createSignal } from "solid-js";
import { TerminalPane } from "./TerminalPane";
import { ReviewPane } from "./ReviewPane";
import { reviewTabs, setWorktreeTab } from "../lib/store";
import type { Worktree } from "../lib/ipc";

export function WorktreePane(props: { worktree: Worktree; active: boolean }) {
  const reviewing = () => reviewTabs()[props.worktree.id] === "review";
  const [terminalSeen, setTerminalSeen] = createSignal(false);
  createEffect(() => { if (!reviewing()) setTerminalSeen(true); });
  return <div class="absolute inset-0 flex flex-col" style={{ visibility: props.active ? "visible" : "hidden", "pointer-events": props.active ? "auto" : "none" }}>
    <Show when={props.worktree.kind !== "orchestrator"}>
      <div class="flex shrink-0 gap-4 px-4 border-b border-[var(--color-border)] bg-[var(--color-bg-elevated)]">
        <ForTab label="Terminal" active={!reviewing()} onClick={() => setWorktreeTab(props.worktree.id, "terminal")} />
        <ForTab label="Revisar" active={reviewing()} onClick={() => setWorktreeTab(props.worktree.id, "review")} />
      </div>
    </Show>
    <div class="relative flex-1 min-h-0">
      <Show when={terminalSeen()}><TerminalPane worktree={props.worktree} active={props.active && !reviewing()} /></Show>
      <Show when={reviewing()}><div class="absolute inset-0"><ReviewPane worktree={props.worktree} /></div></Show>
    </div>
  </div>;
}
function ForTab(props: { label: string; active: boolean; onClick: () => void }) {
  return <button class="py-2 text-xs border-b-2 border-transparent text-[var(--color-fg-muted)] hover:text-[var(--color-fg)]" classList={{ "border-[var(--color-accent)]! text-[var(--color-fg)]!": props.active }} onClick={props.onClick}>{props.label}</button>;
}
