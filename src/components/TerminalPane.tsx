import { createEffect, createSignal, onCleanup, onMount, Show } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  onPtyExit,
  onPtyOutput,
  sessionClose,
  sessionOpen,
  sessionResize,
  sessionWrite,
  worktreeResizeWindow,
  type Worktree,
} from "../lib/ipc";
import { appStore, clearHibernationNote, closePane } from "../lib/store";
import type { UnlistenFn } from "@tauri-apps/api/event";
import type { IDisposable } from "@xterm/xterm";

function bytesToB64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode.apply(
      null,
      bytes.subarray(i, i + chunk) as unknown as number[],
    );
  }
  return btoa(binary);
}

function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function stringToB64(s: string): string {
  return bytesToB64(new TextEncoder().encode(s));
}

/// Connects an xterm.js instance to the tmux session for this worktree. All
/// actual session state (scrollback, mode negotiation, claude process
/// lifetime) lives in the tmux server — this component is a thin client.
export function TerminalPane(props: { worktree: Worktree; active: boolean }) {
  let containerRef!: HTMLDivElement;
  const [status, setStatus] = createSignal<"connecting" | "ready" | "exited">(
    "connecting",
  );
  // Live monitor status for this session — drives the iTerm-style "working" bar.
  const working = () =>
    appStore.statusByWorktree[props.worktree.id] === "working";

  let term: Terminal | null = null;
  let fit: FitAddon | null = null;
  let outputUnlisten: UnlistenFn | null = null;
  let exitUnlisten: UnlistenFn | null = null;
  let resizeObserver: ResizeObserver | null = null;
  let oscDisposable: IDisposable | null = null;

  onMount(async () => {
    term = new Terminal({
      // Match Yani's iTerm "Default" profile: Monaco 12, classic ANSI palette
      // on pure black. Read from ~/Library/Preferences/com.googlecode.iterm2.plist.
      fontFamily: "Monaco, ui-monospace, Menlo, monospace",
      fontSize: 12,
      // iTerm's "Default" profile breathes between lines; xterm packs them at
      // 1.0. 1.2 matches that airier feel (the readability win). Safe with the
      // DOM renderer — the non-integer lineHeight glitch was WebGL-only.
      lineHeight: 1.2,
      cursorBlink: true,
      cursorStyle: "block",
      allowProposedApi: true,
      // OSC 8 hyperlinks — links whose visible text differs from the URL (what
      // Claude emits for "[text](url)" style links). The WebLinksAddon only
      // matches bare URLs, so these need a linkHandler to be clickable. Opens
      // the same way as bare URLs (Cmd+click → openUrl, clipboard fallback).
      linkHandler: {
        activate: (_e, uri) => {
          openUrl(uri).catch(() => {
            navigator.clipboard.writeText(uri).catch(() => {});
          });
        },
      },
      // tmux runs with `mouse on` so a plain drag goes into tmux copy-mode
      // rather than producing an xterm selection. Holding Option bypasses
      // mouse reporting and lets the user make a native xterm selection
      // that Cmd+C can copy. (Shift+drag also works — xterm default.)
      macOptionClickForcesSelection: true,
      // Zero xterm-side scrollback: tmux already keeps a clean 50k-line
      // history (see tmux.conf), and Claude Code's streaming UI uses
      // cursor-up-and-rewrite which pollutes xterm's scrollback with every
      // intermediate redraw frame. By holding nothing, xterm always shows
      // the current screen, and scroll wheel routes into tmux copy mode
      // (clean history) via `mouse on`.
      scrollback: 0,
      theme: {
        background: "#000000",
        foreground: "#ffffff",
        cursor: "#cccccc",
        cursorAccent: "#ffffff",
        selectionBackground: "#b5d5ff",
        selectionForeground: "#000000",
        black: "#000000",
        red: "#bb0000",
        green: "#00bb00",
        yellow: "#bbbb00",
        blue: "#0000bb",
        magenta: "#bb00bb",
        cyan: "#00bbbb",
        white: "#bbbbbb",
        brightBlack: "#555555",
        brightRed: "#ff5555",
        brightGreen: "#55ff55",
        brightYellow: "#ffff55",
        brightBlue: "#5555ff",
        brightMagenta: "#ff55ff",
        brightCyan: "#55ffff",
        brightWhite: "#ffffff",
      },
    });

    fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(
      new WebLinksAddon((_e, uri) => {
        openUrl(uri).catch(() => {
          navigator.clipboard.writeText(uri).catch(() => {});
        });
      }),
    );
    // No WebGL addon: xterm's default DOM renderer. The WebGL renderer
    // corrupts on every write inside the Tauri webview (stale/overlapping
    // glyphs, mis-positioned with non-integer lineHeight) — a fresh GL context
    // from a reload renders once then re-breaks. The DOM renderer is rock-solid
    // and plenty fast for a terminal pane.

    term.open(containerRef);
    fit.fit();

    const worktreeId = props.worktree.id;
    const cols = term.cols;
    const rows = term.rows;

    // Subscribe BEFORE spawning so we don't race and drop early output.
    outputUnlisten = await onPtyOutput((e) => {
      if (e.worktree_id !== worktreeId) return;
      term!.write(b64ToBytes(e.b64));
    });
    exitUnlisten = await onPtyExit((e) => {
      if (e.worktree_id !== worktreeId) return;
      setStatus("exited");
    });

    try {
      await sessionOpen({ worktree_id: worktreeId, cols, rows });
    } catch (e) {
      term.writeln(`\x1b[31mFailed to attach: ${String(e)}\x1b[0m`);
      setStatus("exited");
      return;
    }

    setStatus("ready");

    term.onData((data) => {
      sessionWrite(worktreeId, stringToB64(data)).catch((err) =>
        console.error("write", err),
      );
    });

    // Cmd+C: copy the xterm selection to the OS clipboard (returning `false`
    // suppresses xterm's own handling). Ctrl+C → SIGINT is untouched because we
    // only intercept when metaKey is set without ctrl/alt.
    //
    // Cmd+V is deliberately NOT intercepted: the webview fires its own native
    // paste event, which xterm handles (and wraps in bracketed-paste markers
    // for Claude). Intercepting it to also call `term.paste` made the text
    // arrive twice — so we let the single native paste through.
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;
      if (!e.metaKey || e.ctrlKey || e.altKey) return true;
      const k = e.key.toLowerCase();
      if (k === "c") {
        const sel = term?.getSelection() ?? "";
        if (!sel) return true;
        navigator.clipboard.writeText(sel).catch((err) =>
          console.error("clipboard write", err),
        );
        return false;
      }
      return true;
    });

    // OSC 52 bridge: tmux with `set-clipboard on` emits `ESC]52;c;<base64>ST`
    // after copy-mode yank. Decode and push to the OS clipboard so the
    // tmux-native copy flow (wheel-scroll, drag, `y`) ends up where users
    // expect. Payload format: "<selection>;<base64|?>"; ignore queries ("?").
    oscDisposable = term.parser.registerOscHandler(52, (data) => {
      const semi = data.indexOf(";");
      if (semi < 0) return false;
      const payload = data.slice(semi + 1);
      if (payload === "?" || payload === "") return false;
      try {
        // tmux base64-encodes UTF-8 bytes. atob() alone yields a Latin-1
        // string (é → Ã©), so decode the bytes as UTF-8.
        const text = new TextDecoder().decode(b64ToBytes(payload));
        navigator.clipboard.writeText(text).catch((err) =>
          console.error("clipboard write (osc52)", err),
        );
      } catch {
        // Not valid base64 — drop it silently.
      }
      return true;
    });

    resizeObserver = new ResizeObserver(() => {
      if (!term) return;
      // When a pane is inactive it's visibility:hidden but still in layout.
      // Guard anyway: a 0×0 resize flowing through to tmux corrupts the
      // session's render and you come back to a garbled screen.
      const rect = containerRef.getBoundingClientRect();
      if (rect.width < 10 || rect.height < 10) return;
      fit?.fit();
      if (term.cols > 0 && term.rows > 0) {
        sessionResize(worktreeId, term.cols, term.rows).catch(() => {});
        // Reclaim the desktop's full width on the tmux window — a phone viewer
        // may have narrowed it (window-size manual).
        worktreeResizeWindow(worktreeId, term.cols, term.rows).catch(() => {});
      }
    });
    resizeObserver.observe(containerRef);

    term.focus();
  });

  // Refit + refocus when pane becomes active or when `status` flips to ready
  // (first moment `term` is non-null and the PTY is live).
  createEffect(() => {
    status();
    if (props.active && term) {
      queueMicrotask(() => {
        fit?.fit();
        term?.focus();
        // Reclaim our width: returning to this pane after a phone viewer
        // narrowed the session restores it to the desktop size.
        if (term && term.cols > 0 && term.rows > 0) {
          worktreeResizeWindow(props.worktree.id, term.cols, term.rows).catch(
            () => {},
          );
        }
      });
    }
  });

  onCleanup(() => {
    outputUnlisten?.();
    exitUnlisten?.();
    resizeObserver?.disconnect();
    oscDisposable?.dispose();
    term?.dispose();
    term = null;
    // Tear down the PTY *client* so the Rust reader thread exits and stops
    // emitting pty:output events into the void. The tmux *session* stays
    // alive — Claude keeps running inside tmux, and reopening the pane
    // reattaches. Full session teardown happens in `worktree_remove`.
    sessionClose(props.worktree.id).catch(() => {});
  });

  return (
    <div
      class="absolute inset-0"
      style={{
        // visibility (not display) so inactive panes keep their layout size.
        // A display:none pane would collapse to 0×0, the ResizeObserver would
        // fire, and we'd push a garbage size to tmux.
        visibility: props.active ? "visible" : "hidden",
        "pointer-events": props.active ? "auto" : "none",
      }}
    >
      <Show when={working()}>
        <div class="flock-working-bar" />
      </Show>
      <Show when={appStore.hibernationNoteByWorktree[props.worktree.id]}>
        {(note) => (
          <div class="absolute top-0 inset-x-0 z-10 flex items-start gap-2 px-3 py-2 text-[11px] bg-[var(--color-warn)]/15 text-[var(--color-warn)] border-b border-[var(--color-warn)]/30 pointer-events-auto">
            <span class="flex-1 leading-snug">{note()}</span>
            <button
              class="shrink-0 px-1.5 rounded hover:bg-[var(--color-warn)]/25 transition"
              title="Dismiss"
              onClick={() => clearHibernationNote(props.worktree.id)}
            >
              ✕
            </button>
          </div>
        )}
      </Show>
      <div
        ref={(el) => (containerRef = el)}
        class="absolute inset-0"
        onClick={() => term?.focus()}
      />
      <div
        class="absolute top-2 right-3 text-[10px] font-mono uppercase tracking-wider px-2 py-0.5 rounded pointer-events-none"
        classList={{
          "bg-[var(--color-success)]/15 text-[var(--color-success)]":
            status() === "ready",
          "bg-[var(--color-warn)]/15 text-[var(--color-warn)]":
            status() === "connecting",
          "bg-[var(--color-fg-dim)]/15 text-[var(--color-fg-dim)]":
            status() === "exited",
        }}
      >
        {status()}
      </div>
      {status() === "exited" && (
        <button
          class="absolute bottom-4 right-4 px-3 py-1.5 text-[11px] rounded-md bg-[var(--color-accent)]/20 text-[var(--color-accent)] hover:bg-[var(--color-accent)]/30 transition pointer-events-auto"
          onClick={() => closePane(props.worktree.id)}
        >
          Close pane
        </button>
      )}
    </div>
  );
}
