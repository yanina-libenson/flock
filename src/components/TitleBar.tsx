import type { JSX } from "solid-js";

/// Single-row top bar, iTerm-style: the tab strip and controls live here next
/// to the traffic lights. App.tsx composes the contents; this is just the
/// draggable shell. `padLeft` is the left inset that clears the macOS traffic
/// lights when windowed, and collapses in fullscreen (where they're hidden).
export function TitleBar(props: { padLeft: number; children?: JSX.Element }) {
  return (
    <div
      class="drag-region flex items-center gap-1.5 h-10 shrink-0 border-b border-[var(--color-border)] bg-[var(--color-bg-elevated)]/70 backdrop-blur-lg"
      style={{ "padding-left": `${props.padLeft}px`, "padding-right": "8px" }}
    >
      {props.children}
    </div>
  );
}
