import { createSignal, onMount, Show } from "solid-js";
import { X, Copy, Check, Smartphone } from "lucide-solid";
import { remoteInfo, remoteStart, remoteStop, type RemoteInfo } from "../lib/ipc";

const REMOTE_ENABLED_KEY = "flock.remote.enabled";

/// Persisted intent to run the remote API. Read on app boot to auto-start.
export function remoteEnabledPref(): boolean {
  return localStorage.getItem(REMOTE_ENABLED_KEY) === "true";
}

export function SettingsModal(props: { onClose: () => void }) {
  const [info, setInfo] = createSignal<RemoteInfo | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [revealed, setRevealed] = createSignal(false);
  const [copied, setCopied] = createSignal<string | null>(null);

  onMount(async () => {
    try {
      setInfo(await remoteInfo());
    } catch (e) {
      console.error("remoteInfo failed", e);
    }
  });

  async function toggle() {
    if (busy()) return;
    setBusy(true);
    const turningOn = !info()?.running;
    try {
      const next = turningOn ? await remoteStart() : await remoteStop();
      localStorage.setItem(REMOTE_ENABLED_KEY, String(turningOn));
      setInfo(next);
    } catch (e) {
      console.error("remote toggle failed", e);
      alert(`Couldn't ${turningOn ? "start" : "stop"} remote access:\n${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  async function copy(text: string, key: string) {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(key);
      setTimeout(() => setCopied((c) => (c === key ? null : c)), 1500);
    } catch (e) {
      console.error(e);
    }
  }

  return (
    <div
      class="fixed inset-0 z-[100] flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={props.onClose}
    >
      <div
        class="w-[480px] rounded-xl border border-[var(--color-border-strong)] bg-[var(--color-bg-elevated)] shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div class="flex items-center justify-between px-5 py-3 border-b border-[var(--color-border)]">
          <h2 class="text-[14px] font-semibold text-[var(--color-fg)]">Settings</h2>
          <button
            class="p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-dim)] hover:text-[var(--color-fg)] transition"
            onClick={props.onClose}
          >
            <X size={14} />
          </button>
        </div>

        <div class="p-5">
          <div class="flex items-start gap-3">
            <Smartphone size={16} class="mt-0.5 text-[var(--color-accent)] shrink-0" />
            <div class="flex-1 min-w-0">
              <div class="flex items-center justify-between gap-3">
                <div class="text-[13px] font-medium text-[var(--color-fg)]">
                  Remote access (mobile PWA)
                </div>
                <button
                  role="switch"
                  aria-checked={info()?.running ?? false}
                  disabled={busy()}
                  onClick={toggle}
                  class="relative w-9 h-5 rounded-full transition shrink-0 disabled:opacity-50"
                  classList={{
                    "bg-[var(--color-accent)]": info()?.running ?? false,
                    "bg-[var(--color-border-strong)]": !(info()?.running ?? false),
                  }}
                >
                  <span
                    class="absolute top-0.5 w-4 h-4 rounded-full bg-white transition-all"
                    classList={{
                      "left-[18px]": info()?.running ?? false,
                      "left-0.5": !(info()?.running ?? false),
                    }}
                  />
                </button>
              </div>
              <div class="mt-1 text-[11px] text-[var(--color-fg-dim)] leading-snug">
                Serve the worktree dashboard to your phone. Binds to localhost +
                your Tailscale IP only — never exposed to the public internet.
              </div>
            </div>
          </div>

          <Show when={info()?.running}>
            <div class="mt-4 pt-4 border-t border-[var(--color-border)] space-y-3">
              <div>
                <div class="text-[10px] font-semibold uppercase tracking-wide text-[var(--color-fg-dim)] mb-1">
                  Access token
                </div>
                <div class="flex items-center gap-2">
                  <code class="flex-1 truncate text-[11px] font-mono bg-[var(--color-bg)] border border-[var(--color-border)] rounded px-2 py-1.5 text-[var(--color-fg-muted)]">
                    {revealed() ? info()!.token : "•".repeat(24)}
                  </code>
                  <button
                    class="text-[11px] px-2 py-1.5 rounded border border-[var(--color-border)] text-[var(--color-fg-muted)] hover:text-[var(--color-fg)] hover:bg-[var(--color-bg-hover)] transition"
                    onClick={() => setRevealed((r) => !r)}
                  >
                    {revealed() ? "Hide" : "Show"}
                  </button>
                  <button
                    class="p-1.5 rounded border border-[var(--color-border)] text-[var(--color-fg-muted)] hover:text-[var(--color-fg)] hover:bg-[var(--color-bg-hover)] transition"
                    title="Copy token"
                    onClick={() => copy(info()!.token, "token")}
                  >
                    {copied() === "token" ? <Check size={12} /> : <Copy size={12} />}
                  </button>
                </div>
              </div>

              <div>
                <div class="text-[10px] font-semibold uppercase tracking-wide text-[var(--color-fg-dim)] mb-1">
                  Open on your phone
                </div>
                <div class="space-y-1.5">
                  {info()!.urls.map((url) => (
                    <div class="flex items-center gap-2">
                      <code class="flex-1 truncate text-[11px] font-mono bg-[var(--color-bg)] border border-[var(--color-border)] rounded px-2 py-1.5 text-[var(--color-fg-muted)]">
                        {url.replace(/\?token=.*/, "")}
                      </code>
                      <button
                        class="p-1.5 rounded border border-[var(--color-border)] text-[var(--color-fg-muted)] hover:text-[var(--color-fg)] hover:bg-[var(--color-bg-hover)] transition"
                        title="Copy URL with token"
                        onClick={() => copy(url, url)}
                      >
                        {copied() === url ? <Check size={12} /> : <Copy size={12} />}
                      </button>
                    </div>
                  ))}
                </div>
                <div class="mt-1.5 text-[11px] text-[var(--color-fg-dim)] leading-snug">
                  The copied link includes the token. Open it once on your phone;
                  the token is saved and stripped from the URL.
                </div>
              </div>
            </div>
          </Show>
        </div>
      </div>
    </div>
  );
}
