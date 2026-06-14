import { createSignal, onMount, For, Show } from "solid-js";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  X,
  Copy,
  Check,
  Smartphone,
  Plus,
  Trash2,
  FolderPlus,
  Boxes,
} from "lucide-solid";
import {
  remoteInfo,
  remoteStart,
  remoteStop,
  envConfigGet,
  envConfigSet,
  type RemoteInfo,
  type EnvBinding,
} from "../lib/ipc";

/// One environment in the editor: vars are edited as KEY=VALUE text, parsed on
/// save. Keeps the UI to plain textareas instead of dynamic key/value rows.
interface EnvDraft {
  name: string;
  varsText: string;
}

function parseVars(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const t = line.trim();
    if (!t || t.startsWith("#")) continue;
    const eq = t.indexOf("=");
    if (eq <= 0) continue;
    out[t.slice(0, eq).trim()] = t.slice(eq + 1).trim();
  }
  return out;
}

function formatVars(vars: Record<string, string>): string {
  return Object.entries(vars)
    .map(([k, v]) => `${k}=${v}`)
    .join("\n");
}

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

  const [envs, setEnvs] = createSignal<EnvDraft[]>([]);
  const [bindings, setBindings] = createSignal<EnvBinding[]>([]);
  const [envSaved, setEnvSaved] = createSignal(false);

  onMount(async () => {
    try {
      setInfo(await remoteInfo());
    } catch (e) {
      console.error("remoteInfo failed", e);
    }
    try {
      const cfg = await envConfigGet();
      setEnvs(
        cfg.environments.map((e) => ({
          name: e.name,
          varsText: formatVars(e.vars),
        })),
      );
      setBindings(cfg.bindings);
    } catch (e) {
      console.error("envConfigGet failed", e);
    }
  });

  function addEnv() {
    setEnvs((prev) => [...prev, { name: "", varsText: "" }]);
  }
  function updateEnv(i: number, patch: Partial<EnvDraft>) {
    setEnvs((prev) => prev.map((e, idx) => (idx === i ? { ...e, ...patch } : e)));
    setEnvSaved(false);
  }
  function removeEnv(i: number) {
    setEnvs((prev) => prev.filter((_, idx) => idx !== i));
    setEnvSaved(false);
  }

  async function addBinding() {
    const picked = await openDialog({
      directory: true,
      multiple: false,
      title: "Pick a folder (or a repo) for this environment",
    });
    if (!picked) return;
    const path = Array.isArray(picked) ? picked[0] : picked;
    const first = envs()[0]?.name ?? "";
    setBindings((prev) => [...prev, { path, env: first }]);
    setEnvSaved(false);
  }
  function updateBinding(i: number, patch: Partial<EnvBinding>) {
    setBindings((prev) => prev.map((b, idx) => (idx === i ? { ...b, ...patch } : b)));
    setEnvSaved(false);
  }
  function removeBinding(i: number) {
    setBindings((prev) => prev.filter((_, idx) => idx !== i));
    setEnvSaved(false);
  }

  async function saveEnvConfig() {
    try {
      await envConfigSet({
        environments: envs()
          .filter((e) => e.name.trim())
          .map((e) => ({ name: e.name.trim(), vars: parseVars(e.varsText) })),
        bindings: bindings().filter((b) => b.path && b.env),
      });
      setEnvSaved(true);
      setTimeout(() => setEnvSaved(false), 1500);
    } catch (e) {
      console.error("envConfigSet failed", e);
      alert(`Couldn't save environments:\n${String(e)}`);
    }
  }

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

        <div class="p-5 max-h-[78vh] overflow-y-auto">
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

          {/* ---------- Environments ---------- */}
          <div class="mt-5 pt-5 border-t border-[var(--color-border)]">
            <div class="flex items-start gap-3">
              <Boxes size={16} class="mt-0.5 text-[var(--color-accent)] shrink-0" />
              <div class="flex-1 min-w-0">
                <div class="text-[13px] font-medium text-[var(--color-fg)]">
                  Environments
                </div>
                <div class="mt-1 text-[11px] text-[var(--color-fg-dim)] leading-snug">
                  Env vars (e.g. MCP tokens) injected into Claude per folder. A
                  binding applies to every repo under that folder; a deeper
                  binding wins. Stored locally, never committed. Changing an
                  environment takes effect next time you open the worktree.
                </div>
              </div>
            </div>

            <div class="mt-3 space-y-3">
              <For each={envs()}>
                {(env, i) => (
                  <div class="rounded-md border border-[var(--color-border)] bg-[var(--color-bg)]/50 p-2.5">
                    <div class="flex items-center gap-2 mb-1.5">
                      <input
                        class="flex-1 bg-[var(--color-bg)] border border-[var(--color-border)] rounded px-2 py-1 text-[12px] font-medium text-[var(--color-fg)] outline-none"
                        placeholder="Environment name (e.g. Personal-A)"
                        value={env.name}
                        onInput={(e) => updateEnv(i(), { name: e.currentTarget.value })}
                      />
                      <button
                        class="p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-dim)] hover:text-[var(--color-danger)] transition"
                        title="Delete environment"
                        onClick={() => removeEnv(i())}
                      >
                        <Trash2 size={13} />
                      </button>
                    </div>
                    <textarea
                      class="w-full h-16 bg-[var(--color-bg)] border border-[var(--color-border)] rounded px-2 py-1 text-[11px] font-mono text-[var(--color-fg-muted)] outline-none resize-y"
                      placeholder="RENDER_API_KEY=rnd_xxx"
                      value={env.varsText}
                      onInput={(e) => updateEnv(i(), { varsText: e.currentTarget.value })}
                    />
                  </div>
                )}
              </For>
              <button
                class="flex items-center gap-1.5 text-[11px] text-[var(--color-accent)] hover:underline"
                onClick={addEnv}
              >
                <Plus size={12} /> Add environment
              </button>
            </div>

            <div class="mt-4">
              <div class="text-[10px] font-semibold uppercase tracking-wide text-[var(--color-fg-dim)] mb-1.5">
                Folder bindings
              </div>
              <div class="space-y-1.5">
                <For
                  each={bindings()}
                  fallback={
                    <div class="text-[11px] text-[var(--color-fg-dim)]">
                      No bindings yet.
                    </div>
                  }
                >
                  {(b, i) => (
                    <div class="flex items-center gap-2">
                      <code
                        class="flex-1 truncate text-[11px] font-mono bg-[var(--color-bg)] border border-[var(--color-border)] rounded px-2 py-1.5 text-[var(--color-fg-muted)]"
                        title={b.path}
                      >
                        {b.path}
                      </code>
                      <select
                        class="text-[11px] bg-[var(--color-bg)] border border-[var(--color-border)] rounded px-1.5 py-1.5 text-[var(--color-fg)] outline-none"
                        value={b.env}
                        onChange={(e) => updateBinding(i(), { env: e.currentTarget.value })}
                      >
                        <For each={envs()}>
                          {(env) => (
                            <option value={env.name}>{env.name || "(unnamed)"}</option>
                          )}
                        </For>
                      </select>
                      <button
                        class="p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-fg-dim)] hover:text-[var(--color-danger)] transition"
                        title="Delete binding"
                        onClick={() => removeBinding(i())}
                      >
                        <Trash2 size={13} />
                      </button>
                    </div>
                  )}
                </For>
              </div>
              <button
                class="mt-2 flex items-center gap-1.5 text-[11px] text-[var(--color-accent)] hover:underline"
                onClick={addBinding}
              >
                <FolderPlus size={12} /> Add folder binding
              </button>
            </div>

            <div class="mt-4 flex justify-end">
              <button
                class="px-3 py-1.5 text-[12px] font-medium rounded-md bg-[var(--color-accent)] text-black hover:opacity-90 transition"
                onClick={saveEnvConfig}
              >
                {envSaved() ? "Saved ✓" : "Save environments"}
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
