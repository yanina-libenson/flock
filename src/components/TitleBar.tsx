import type { JSX } from "solid-js";

export function TitleBar(props: { children?: JSX.Element }) {
  return (
    <div
      class="drag-region flex items-center h-9 shrink-0 border-b border-[var(--color-border)] bg-[var(--color-bg-elevated)]/70 backdrop-blur-lg"
      style={{ "padding-left": "78px" /* traffic-light space */ }}
    >
      <div class="flex items-center gap-2 text-[11px] font-semibold tracking-wide text-[var(--color-fg-muted)] uppercase">
        <span
          class="inline-block w-2 h-2 rounded-full"
          style={{ background: "var(--color-accent)" }}
        />
        <span>Flock</span>
      </div>
      <div class="flex-1" />
      <div class="no-drag flex items-center gap-2 px-3">{props.children}</div>
    </div>
  );
}
