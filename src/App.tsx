import {
  For,
  Show,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import { TitleBar } from "./components/TitleBar";
import { Sidebar } from "./components/Sidebar";
import { TerminalPane } from "./components/TerminalPane";
import { PrFooter } from "./components/PrFooter";
import { NewWorktreeModal } from "./components/NewWorktreeModal";
import { NewOrchestratorModal } from "./components/NewOrchestratorModal";
import { SettingsModal, remoteEnabledPref } from "./components/SettingsModal";
import {
  appStore,
  closePane,
  openPane,
  setActivePane,
  setWorktreeStatus,
  clearWorktreeStatus,
  setWorktreePrStatus,
  applyWorktreeTitle,
  addWorktree,
  hibernatePane,
  jumpToNextNeedingInput,
  worktreesNeedingInput,
  sidebarVisible,
  toggleSidebar,
} from "./lib/store";
import {
  tmuxCheck,
  onWorktreeStatus,
  onWorktreeTitle,
  onWorktreePrStatus,
  onWorktreeHibernated,
  onWorktreeCreated,
  onPtyExit,
  setActiveWorktree,
  sessionWriteText,
  remoteStart,
  worktreeLabel,
  type Repo,
  type Worktree,
} from "./lib/ipc";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
  onAction,
} from "@tauri-apps/plugin-notification";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  GitBranch,
  Network,
  Settings as SettingsIcon,
  PanelLeftClose,
  PanelLeftOpen,
} from "lucide-solid";

function App() {
  const [modalRepo, setModalRepo] = createSignal<Repo | null>(null);
  const [showOrchestrator, setShowOrchestrator] = createSignal(false);
  const [showSettings, setShowSettings] = createSignal(false);
  // null = still checking; true/false once we know.
  const [tmuxOk, setTmuxOk] = createSignal<boolean | null>(null);
  // macOS hides the traffic lights in fullscreen, leaving the top-left empty —
  // we fill it with the Flock wordmark and drop the inset there.
  const [isFullscreen, setIsFullscreen] = createSignal(false);

  const worktreesById = createMemo(() => {
    const m = new Map<number, Worktree>();
    for (const repoId of Object.keys(appStore.worktreesByRepo)) {
      const list = appStore.worktreesByRepo[Number(repoId)] ?? [];
      for (const w of list) m.set(w.id, w);
    }
    // Orchestrators are pane-openable too (tabs, labels, drag-drop target).
    for (const o of appStore.orchestrators) m.set(o.id, o);
    return m;
  });

  // The worktree (or orchestrator) of the active pane — its full title is shown
  // in the top bar, since we don't render per-tab chips anymore.
  const activeWorktree = createMemo(() => {
    const id = appStore.activePaneId;
    return id == null ? null : (worktreesById().get(id) ?? null);
  });

  onMount(() => {
    tmuxCheck()
      .then(setTmuxOk)
      .catch(() => setTmuxOk(false));

    // Track fullscreen so the top bar can swap traffic-light space for the
    // Flock wordmark. onResized fires on the fullscreen enter/exit transition.
    const win = getCurrentWindow();
    win.isFullscreen().then(setIsFullscreen).catch(() => {});
    const fsUnlisten = win.onResized(() => {
      win.isFullscreen().then(setIsFullscreen).catch(() => {});
    });
    onCleanup(() => fsUnlisten.then((f) => f()));

    // Resume remote access if it was on last session.
    if (remoteEnabledPref()) {
      remoteStart().catch((e) => console.error("remoteStart on boot failed", e));
    }

    // Ask for notification permission once, up front, so the first
    // needs-input event can actually surface a banner.
    (async () => {
      try {
        if (!(await isPermissionGranted())) {
          await requestPermission();
        }
      } catch {
        /* notifications unavailable — degrade silently */
      }
    })();

    // Backend monitor pushes one event per status change. needs_input is the
    // attention signal: notify unless you're already looking at that pane.
    // Per-worktree timing to keep notifications meaningful (not flicker).
    const workingSince = new Map<number, number>();
    const lastNotified = new Map<number, number>();
    // Most recent worktree we notified about, and whether a notification-click
    // jump is still "armed". Clicking a notification activates the app (window
    // regains focus); if that happens shortly after we notified, jump to that
    // worktree. This works regardless of whether onAction fires (flaky on macOS).
    let lastNotifiedWorktree: number | null = null;
    let pendingJump = false;
    const jumpTo = (wid: number | null) => {
      pendingJump = false;
      if (wid != null) openPane(wid);
    };
    const MIN_WORK_MS = 8000; // ignore working blips (focus redraws, quick edits)
    const COOLDOWN_MS = 30000; // at most one ping per worktree per 30s

    const statusUnlisten = onWorktreeStatus((e) => {
      const prev = appStore.statusByWorktree[e.worktree_id];
      setWorktreeStatus(e.worktree_id, e.status);
      const id = e.worktree_id;
      const now = Date.now();

      if (e.status === "working") {
        if (prev !== "working") workingSince.set(id, now);
        return;
      }

      // Now idle or needs_input. How long was it actually working?
      const workedMs = workingSince.has(id) ? now - workingSince.get(id)! : 0;
      workingSince.delete(id);

      const asked = e.status === "needs_input" && prev !== "needs_input";
      // "Finished" only after sustained work — filters the working↔idle flicker
      // that switching panes/apps causes (tmux focus events make Claude redraw).
      const finished =
        e.status === "idle" && prev === "working" && workedMs >= MIN_WORK_MS;
      if (!asked && !finished) return;
      if (now - (lastNotified.get(id) ?? 0) < COOLDOWN_MS) return;

      const lookingHere =
        appStore.activePaneId === id && document.hasFocus();
      if (lookingHere) return;

      lastNotified.set(id, now);
      lastNotifiedWorktree = id;
      // Arm the focus-jump for a short window; clear it so a later manual
      // focus doesn't hijack to this worktree.
      pendingJump = true;
      setTimeout(() => {
        pendingJump = false;
      }, 12000);
      const w = worktreesById().get(id);
      // The task title (auto-generated) is what's meaningful — fall back to the
      // branch only if there's no title yet.
      const label = w ? worktreeLabel(w) : `worktree ${id}`;
      try {
        sendNotification({
          title: asked ? "Claude needs your input" : "Claude finished",
          body: label,
          // Gentle macOS chime. Alternatives if you want softer/different:
          // Tink, Pop, Purr (subtle) · Hero, Submarine (deeper) · Bottle, Frog.
          sound: "Glass",
          // Carried back to onAction so a click jumps to this worktree.
          extra: { worktreeId: id },
        });
      } catch {
        /* permission denied or unavailable */
      }
    });
    const titleUnlisten = onWorktreeTitle((e) =>
      applyWorktreeTitle(e.worktree_id, e.title),
    );
    const exitUnlisten = onPtyExit((e) => clearWorktreeStatus(e.worktree_id));
    // A worktree appeared out-of-band (spawned by an orchestrator, the MCP,
    // cron, or the REST API) — add it live so it shows in the sidebar without a
    // manual refresh.
    const createdUnlisten = onWorktreeCreated((w) => addWorktree(w));
    const prStatusUnlisten = onWorktreePrStatus((e) =>
      setWorktreePrStatus(e.worktree_id, e.status),
    );
    // Monitor reaped a session to save memory — drop its pane to a dormant
    // tab. Re-activating it (clicking the tab) reattaches and resumes. A
    // "memory" reap carries a note so the reopened pane explains the kill.
    const hibernateUnlisten = onWorktreeHibernated((e) =>
      hibernatePane(
        e.worktree_id,
        e.reason === "memory"
          ? `Flock hibernated this session to free memory${
              e.detail ? ` (was using ${e.detail})` : ""
            }. Resumed from the on-disk transcript.`
          : undefined,
      ),
    );
    // Clicking a notification jumps to its worktree. Primary: onAction (when it
    // fires). Reliable fallback: the window regaining focus while a jump is
    // armed (a notification click activates the app).
    const actionUnlisten = onAction((n) => {
      const raw = (n.extra as { worktreeId?: unknown } | undefined)?.worktreeId;
      const fromExtra = raw != null && !isNaN(Number(raw)) ? Number(raw) : null;
      jumpTo(fromExtra ?? lastNotifiedWorktree);
    });
    const focusUnlisten = getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      if (focused && pendingJump) jumpTo(lastNotifiedWorktree);
    });
    // Drag a file (e.g. an image) onto the window → write its path into the
    // active session's input, like dropping a file into a terminal. Tauri v2
    // intercepts OS file drops (the DOM never sees them), so we handle its
    // event and forward the path(s) to the PTY. Paths with spaces/specials are
    // single-quoted so each arrives as one token.
    const quotePath = (p: string) =>
      /[^\w@%+=:,./-]/.test(p) ? `'${p.replace(/'/g, "'\\''")}'` : p;
    const dragUnlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type !== "drop") return;
      const id = appStore.activePaneId;
      if (id === null) return;
      const paths = event.payload.paths ?? [];
      if (paths.length === 0) return;
      sessionWriteText(id, paths.map(quotePath).join(" ") + " ").catch(() => {});
    });
    onCleanup(() => {
      statusUnlisten.then((f) => f());
      titleUnlisten.then((f) => f());
      exitUnlisten.then((f) => f());
      createdUnlisten.then((f) => f());
      prStatusUnlisten.then((f) => f());
      hibernateUnlisten.then((f) => f());
      actionUnlisten.then((l) => l.unregister());
      focusUnlisten.then((f) => f());
      dragUnlisten.then((f) => f());
    });

    const handler = (e: KeyboardEvent) => {
      if (!e.metaKey) return;
      if (e.key === "j" || e.key === "J") {
        e.preventDefault();
        jumpToNextNeedingInput();
        return;
      }
      if (e.key === "b" || e.key === "B") {
        e.preventDefault();
        toggleSidebar();
        return;
      }
      if (e.key === "w") {
        if (appStore.activePaneId !== null) {
          e.preventDefault();
          closePane(appStore.activePaneId);
        }
        return;
      }
      if (/^[1-9]$/.test(e.key)) {
        const idx = parseInt(e.key, 10) - 1;
        const id = appStore.openPaneIds[idx];
        if (id !== undefined) {
          e.preventDefault();
          setActivePane(id);
        }
        return;
      }
      if ((e.key === "ArrowLeft" || e.key === "ArrowRight") && e.altKey) {
        const ids = appStore.openPaneIds;
        if (ids.length === 0) return;
        const cur = appStore.activePaneId;
        let idx = cur === null ? 0 : ids.indexOf(cur);
        idx += e.key === "ArrowLeft" ? -1 : 1;
        idx = (idx + ids.length) % ids.length;
        e.preventDefault();
        setActivePane(ids[idx]);
      }
    };
    window.addEventListener("keydown", handler);
    onCleanup(() => window.removeEventListener("keydown", handler));
  });

  // Keep the backend's notion of the focused pane in sync so the idle-
  // hibernation monitor never reaps the worktree you're looking at.
  createEffect(() => {
    setActiveWorktree(appStore.activePaneId).catch(() => {});
  });

  return (
    <div class="flex flex-col h-screen w-screen overflow-hidden bg-[var(--color-bg)] text-[var(--color-fg)]">
      <TitleBar padLeft={isFullscreen() ? 10 : 80}>
        {/* In fullscreen the traffic lights vanish — fill the gap with the
            Flock wordmark so the top-left isn't empty. */}
        <Show when={isFullscreen()}>
          <div class="flex items-center gap-2 shrink-0 pr-1 text-[11px] font-semibold tracking-wide text-[var(--color-fg-muted)] uppercase select-none">
            <span
              class="inline-block w-2 h-2 rounded-full"
              style={{ background: "var(--color-accent)" }}
            />
            <span>Flock</span>
          </div>
        </Show>
        {/* Sidebar toggle, top-left — like VS Code / Zed. */}
        <button
          class="no-drag p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition shrink-0"
          title={sidebarVisible() ? "Hide sidebar (⌘B)" : "Show sidebar (⌘B)"}
          onClick={() => toggleSidebar()}
        >
          <Show when={sidebarVisible()} fallback={<PanelLeftOpen size={15} />}>
            <PanelLeftClose size={15} />
          </Show>
        </button>
        {/* Active session — full title, since there are no per-tab chips.
            A live status dot (working / needs-you), the type icon, then the
            title. Truncates only when the bar fills; hover shows the full
            title. Empty bar space stays draggable. */}
        <div class="flex items-center gap-2.5 min-w-0 flex-1 self-stretch pl-1">
          <Show when={activeWorktree()}>
            {(w) => {
              const st = () => appStore.statusByWorktree[w().id];
              return (
                <div class="flex items-center gap-2 min-w-0">
                  <Show when={st() === "working" || st() === "needs_input"}>
                    <span
                      class="shrink-0 w-1.5 h-1.5 rounded-full animate-pulse"
                      classList={{
                        "bg-[var(--color-accent)]": st() === "working",
                        "bg-[var(--color-warn)]": st() === "needs_input",
                      }}
                    />
                  </Show>
                  {w().kind === "orchestrator" ? (
                    <Network
                      size={14}
                      class="shrink-0 text-[var(--color-accent)]"
                    />
                  ) : (
                    <GitBranch
                      size={14}
                      class="shrink-0 text-[var(--color-fg-dim)]"
                    />
                  )}
                  <span
                    class="truncate text-[13px] font-medium tracking-tight text-[var(--color-fg)]"
                    title={worktreeLabel(w())}
                  >
                    {worktreeLabel(w())}
                  </span>
                </div>
              );
            }}
          </Show>
        </div>
        <WaitingIndicator />
        <button
          class="no-drag p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition shrink-0"
          title="Settings"
          onClick={() => setShowSettings(true)}
        >
          <SettingsIcon size={14} />
        </button>
      </TitleBar>
      <div class="flex flex-1 min-h-0">
        <Show when={sidebarVisible()}>
          <Sidebar
            onCreateWorktree={(r) => setModalRepo(r)}
            onCreateOrchestrator={() => setShowOrchestrator(true)}
          />
        </Show>
        <main class="flex-1 flex flex-col min-w-0">
          <Show
            when={appStore.openPaneIds.length > 0}
            fallback={<EmptyState />}
          >
            <div class="flex-1 relative min-h-0 bg-black">
              <For each={appStore.openPaneIds}>
                {(id) => {
                  const w = () => worktreesById().get(id);
                  // Lazy attach: only mount (and thus attach claude) once the
                  // pane has been activated this session. Dormant tabs — never
                  // opened this launch, or hibernated for memory — render
                  // nothing until clicked, which re-activates and resumes them.
                  return (
                    <Show when={w() && appStore.activatedPaneIds.includes(id)}>
                      <TerminalPane
                        worktree={w()!}
                        active={appStore.activePaneId === id}
                      />
                    </Show>
                  );
                }}
              </For>
            </div>
            {/* Persistent PR list for the active pane — a worktree's own PRs,
                or an orchestrator's whole-fleet aggregation. Collapses to zero
                height when there are none, so it never steals terminal space. */}
            <Show when={activeWorktree()}>
              {(w) => <PrFooter worktree={w()} />}
            </Show>
          </Show>
        </main>
      </div>
      <Show when={modalRepo()}>
        <NewWorktreeModal
          repo={modalRepo()!}
          onClose={() => setModalRepo(null)}
        />
      </Show>
      <Show when={showOrchestrator()}>
        <NewOrchestratorModal onClose={() => setShowOrchestrator(false)} />
      </Show>
      <Show when={showSettings()}>
        <SettingsModal onClose={() => setShowSettings(false)} />
      </Show>
      <Show when={tmuxOk() === false}>
        <TmuxMissingModal />
      </Show>
    </div>
  );
}

/// Title-bar pill showing how many agents are waiting on you. Click (or Cmd+J)
/// cycles to the next one. Hidden when nobody needs input.
function WaitingIndicator() {
  const count = createMemo(() => worktreesNeedingInput().length);
  return (
    <Show when={count() > 0}>
      <button
        class="no-drag flex items-center gap-1.5 px-2 py-1 rounded-md text-[11px] font-medium text-[var(--color-warn)] bg-[var(--color-warn)]/12 hover:bg-[var(--color-warn)]/20 transition"
        title="Jump to the next agent waiting for you (⌘J)"
        onClick={() => jumpToNextNeedingInput()}
      >
        <span class="w-1.5 h-1.5 rounded-full bg-[var(--color-warn)] animate-pulse" />
        {count()} waiting
      </button>
    </Show>
  );
}

function EmptyState() {
  return (
    <div class="flex-1 flex items-center justify-center">
      <div class="text-center max-w-sm px-6">
        <div
          class="mx-auto mb-5 w-14 h-14 rounded-2xl flex items-center justify-center"
          style={{
            background:
              "linear-gradient(135deg, var(--color-accent) 0%, var(--color-accent-muted) 100%)",
            "box-shadow":
              "0 8px 32px -8px var(--color-accent), inset 0 1px 0 rgba(255,255,255,0.15)",
          }}
        >
          <GitBranch size={28} class="text-black" />
        </div>
        <h1 class="text-[18px] font-semibold text-[var(--color-fg)] mb-1.5">
          A flock of Claudes.
        </h1>
        <p class="text-[12.5px] text-[var(--color-fg-muted)] leading-relaxed">
          Add a repository on the left, then create branch worktrees to run
          parallel Claude Code sessions in isolation.
        </p>
      </div>
    </div>
  );
}

function TmuxMissingModal() {
  return (
    <div class="fixed inset-0 z-[100] flex items-center justify-center bg-black/60 backdrop-blur-sm">
      <div class="w-[460px] rounded-xl border border-[var(--color-border-strong)] bg-[var(--color-bg-elevated)] shadow-2xl p-6">
        <h2 class="text-[14px] font-semibold text-[var(--color-fg)] mb-2">
          tmux is required
        </h2>
        <p class="text-[12.5px] text-[var(--color-fg-muted)] leading-relaxed mb-4">
          Flock uses tmux to keep your Claude sessions alive across tab
          switches and app restarts. Install it once, then relaunch Flock:
        </p>
        <pre class="text-[12px] font-mono bg-[var(--color-bg)] border border-[var(--color-border)] rounded-md px-3 py-2 text-[var(--color-fg)]">
brew install tmux
        </pre>
      </div>
    </div>
  );
}

export default App;
