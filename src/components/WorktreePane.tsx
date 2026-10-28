import { Show, createEffect, createSignal } from "solid-js";
import { TerminalPane } from "./TerminalPane";
import { ReviewPane } from "./ReviewPane";
import { reviewTabs, setWorktreeTab, deepSessions } from "../lib/store";
import type { Worktree } from "../lib/ipc";

export function WorktreePane(props: { worktree: Worktree; active: boolean }) {
  const mode = () => reviewTabs()[props.worktree.id] ?? (props.worktree.kind === "review" ? "review" : "terminal");
  const reviewing = () => mode() === "review";
  const deepSession = () => deepSessions()[props.worktree.id];
  const [terminalSeen, setTerminalSeen] = createSignal(false);
  createEffect(() => { if (mode() === "terminal" && props.worktree.kind !== "review") setTerminalSeen(true); });
  return <div class="absolute inset-0 flex flex-col" style={{ visibility: props.active ? "visible" : "hidden", "pointer-events": props.active ? "auto" : "none" }}>
    <Show when={props.worktree.kind !== "orchestrator"}>
      <div class="flex shrink-0 gap-4 px-4 border-b border-[var(--color-border)] bg-[var(--color-bg-elevated)]">
        <Show when={props.worktree.kind !== "review"}><ForTab label="Terminal" active={mode() === "terminal"} onClick={() => setWorktreeTab(props.worktree.id, "terminal")} /></Show>
        <ForTab label="Revisar" active={reviewing()} onClick={() => setWorktreeTab(props.worktree.id, "review")} />
        <Show when={deepSession()}><ForTab label="Revisión profunda" active={mode() === "deep"} onClick={() => setWorktreeTab(props.worktree.id,"deep")} /></Show>
        <Show when={props.worktree.kind === "review"}><span class="ml-auto self-center text-[11px] text-[var(--color-fg-dim)]">De otra persona · solo lectura</span></Show>
      </div>
    </Show>
    <div class="relative flex-1 min-h-0">
      <Show when={terminalSeen()}><TerminalPane worktree={props.worktree} active={props.active && mode() === "terminal"} /></Show>
      <Show when={deepSession()}>{session => <TerminalPane worktree={session()} active={props.active && mode() === "deep"} />}</Show>
      <Show when={reviewing()}><div class="absolute inset-0"><ReviewPane worktree={props.worktree} /></div></Show>
    </div>
  </div>;
}
function ForTab(props: { label: string; active: boolean; onClick: () => void }) {
  return <button class="py-2 text-xs border-b-2 border-transparent text-[var(--color-fg-muted)] hover:text-[var(--color-fg)]" classList={{ "border-[var(--color-accent)]! text-[var(--color-fg)]!": props.active }} onClick={props.onClick}>{props.label}</button>;
}
