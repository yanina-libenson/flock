import {
  For,
  Show,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import { TitleBar } from "./components/TitleBar";
import { Sidebar } from "./components/Sidebar";
import { TabBar } from "./components/TabBar";
import { TerminalPane } from "./components/TerminalPane";
import { NewWorktreeModal } from "./components/NewWorktreeModal";
import { SettingsModal, remoteEnabledPref } from "./components/SettingsModal";
import {
  appStore,
  closePane,
  setActivePane,
  setWorktreeStatus,
  clearWorktreeStatus,
  applyWorktreeTitle,
  jumpToNextNeedingInput,
  worktreesNeedingInput,
} from "./lib/store";
import {
  tmuxCheck,
  onWorktreeStatus,
  onWorktreeTitle,
  onPtyExit,
  remoteStart,
  type Repo,
  type Worktree,
} from "./lib/ipc";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { GitBranch, Settings as SettingsIcon } from "lucide-solid";

function App() {
  const [modalRepo, setModalRepo] = createSignal<Repo | null>(null);
  const [showSettings, setShowSettings] = createSignal(false);
  // null = still checking; true/false once we know.
  const [tmuxOk, setTmuxOk] = createSignal<boolean | null>(null);

  const worktreesById = createMemo(() => {
    const m = new Map<number, Worktree>();
    for (const repoId of Object.keys(appStore.worktreesByRepo)) {
      const list = appStore.worktreesByRepo[Number(repoId)] ?? [];
      for (const w of list) m.set(w.id, w);
    }
    return m;
  });

  onMount(() => {
    tmuxCheck()
      .then(setTmuxOk)
      .catch(() => setTmuxOk(false));

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
    const statusUnlisten = onWorktreeStatus((e) => {
      const prev = appStore.statusByWorktree[e.worktree_id];
      setWorktreeStatus(e.worktree_id, e.status);
      if (e.status !== "needs_input" || prev === "needs_input") return;
      const lookingHere =
        appStore.activePaneId === e.worktree_id && document.hasFocus();
      if (lookingHere) return;
      const w = worktreesById().get(e.worktree_id);
      const repo = w
        ? appStore.repos.find((r) => r.id === w.repo_id)
        : undefined;
      const label = w
        ? `${repo?.name ? `${repo.name}/` : ""}${w.branch}`
        : `worktree ${e.worktree_id}`;
      try {
        sendNotification({ title: "Claude needs you", body: label });
      } catch {
        /* permission denied or unavailable */
      }
    });
    const titleUnlisten = onWorktreeTitle((e) =>
      applyWorktreeTitle(e.worktree_id, e.title),
    );
    const exitUnlisten = onPtyExit((e) => clearWorktreeStatus(e.worktree_id));
    onCleanup(() => {
      statusUnlisten.then((f) => f());
      titleUnlisten.then((f) => f());
      exitUnlisten.then((f) => f());
    });

    const handler = (e: KeyboardEvent) => {
      if (!e.metaKey) return;
      if (e.key === "j" || e.key === "J") {
        e.preventDefault();
        jumpToNextNeedingInput();
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

  return (
    <div class="flex flex-col h-screen w-screen overflow-hidden bg-[var(--color-bg)] text-[var(--color-fg)]">
      <TitleBar>
        <WaitingIndicator />
        <button
          class="no-drag p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition"
          title="Settings"
          onClick={() => setShowSettings(true)}
        >
          <SettingsIcon size={14} />
        </button>
      </TitleBar>
      <div class="flex flex-1 min-h-0">
        <Sidebar onCreateWorktree={(r) => setModalRepo(r)} />
        <main class="flex-1 flex flex-col min-w-0">
          <Show
            when={appStore.openPaneIds.length > 0}
            fallback={<EmptyState />}
          >
            <TabBar worktreesById={worktreesById} />
            <div class="flex-1 relative min-h-0 bg-[var(--color-bg)]">
              <For each={appStore.openPaneIds}>
                {(id) => {
                  const w = () => worktreesById().get(id);
                  return (
                    <Show when={w()}>
                      <TerminalPane
                        worktree={w()!}
                        active={appStore.activePaneId === id}
                      />
                    </Show>
                  );
                }}
              </For>
            </div>
          </Show>
        </main>
      </div>
      <Show when={modalRepo()}>
        <NewWorktreeModal
          repo={modalRepo()!}
          onClose={() => setModalRepo(null)}
        />
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
