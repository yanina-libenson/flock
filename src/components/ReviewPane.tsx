import { For, Show, createMemo, createSignal, onCleanup, onMount } from "solid-js";
import { openUrl } from "@tauri-apps/plugin-opener";
import { appStore } from "../lib/store";
import { reviewPrUrl, reviewLoad, onReviewChanged, type ReviewEval, type ReviewFile, type ReviewRange, type ReviewView, type Worktree } from "../lib/ipc";

const heading = "text-lg uppercase tracking-wider font-semibold text-[var(--color-fg-muted)]";
const column = "min-w-0 overflow-auto p-5 space-y-5 [&_p]:max-w-[70ch]";
const kinds = { comment: "Comentario", naming: "Nombre", logic: "Lógica", test: "Test", scope: "Alcance", bug: "Error", ok: "Bien" };
export const money = (n: number) => `US$${n.toFixed(4)}`;

export function ReviewPane(props: { worktree: Worktree }) {
  const [view, setView] = createSignal<ReviewView>();
  const [fallbackPrUrl, setFallbackPrUrl] = createSignal<string | null>(null);
  const [error, setError] = createSignal("");
  const [loading, setLoading] = createSignal(true);
  const [selected, setSelected] = createSignal<string | null>(null);
  const [highlight, setHighlight] = createSignal<ReviewRange | null>(null);
  const review = () => view()?.review;
  const prUrl = () => review()?.target.pr_url || appStore.prStatusByWorktree[props.worktree.id]?.url || fallbackPrUrl();
  const prLabel = () => {
    const match = prUrl()?.match(/^https:\/\/[^/]+\/([^/]+\/[^/]+)\/pull\/(\d+)/);
    return match ? `PR #${match[2]} · ${match[1]}` : "Ver PR en GitHub";
  };
  const stale = () => !!review() && review()!.target.head_sha !== view()?.head_sha;
  const file = createMemo(() => review()?.files.find(f => f.path === selected()));
  const diff = createMemo(() => view()?.files.find(f => f.path === selected()));
  const numbers = createMemo(() => {
    const result = new Map<string,number>(); let n=0;
    for (const f of review()?.files ?? []) for (const e of f.evals) if(e.severity !== "ok") result.set(`${f.path}:${e.id}`,++n);
    return result;
  });
  const number = (f: ReviewFile, e: ReviewEval) => numbers().get(`${f.path}:${e.id}`);
  const issues = () => review()?.files.flatMap(f => f.evals.filter(e => e.severity !== "ok").map(e => ({ f, e }))) ?? [];
  async function load(deep?: boolean) {
    try { setView(await reviewLoad(props.worktree.id,deep)); }
    catch (e) { setError(String(e)); }
    finally { setLoading(false); }
  }
  onMount(() => {
    void load().then(async () => {
      if (props.worktree.kind !== "review" && !prUrl()) {
        setFallbackPrUrl(await reviewPrUrl(props.worktree.id).catch(() => null));
      }
    });
    const listener = onReviewChanged(id => { if (id === props.worktree.id) void load(); });
    onCleanup(() => { void listener.then(f => f()); });
  });
  function EvalCard(p: { eval: ReviewEval; file: ReviewFile; showFile?: boolean }) {
    const e = () => p.eval;
    return <article class="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-elevated)] p-4 space-y-3 hover:border-[var(--color-accent)]"
      onMouseEnter={() => setHighlight(e())} onMouseLeave={() => setHighlight(null)} onFocusIn={() => setHighlight(e())} onFocusOut={() => setHighlight(null)}>
      <div class="flex gap-2 items-center text-[13px] text-[var(--color-fg-muted)]">
        <Show when={e().severity !== "ok"}><span class="font-mono font-semibold text-[var(--color-fg)]" aria-label={`Evaluación ${number(p.file,e())}`}>{number(p.file,e())}</span></Show>
        <span class="rounded-full px-2 py-0.5 font-semibold" classList={{ "text-[var(--color-warn)] bg-[var(--color-warn)]/10": e().severity === "warn", "text-[var(--color-accent)] bg-[var(--color-accent)]/10": e().severity === "info", "text-[var(--color-success)] bg-[var(--color-success)]/10": e().severity === "ok" }}>{kinds[e().kind]}</span>
        <span>{p.showFile ? `${p.file.name_es} · ` : ""}línea {e().start}{e().end !== e().start ? `–${e().end}` : ""}{e().side === "LEFT" ? " (antes)" : ""}</span>
      </div>
      <p class="text-base leading-relaxed">{e().text_es}</p>
      <Show when={e().severity !== "ok"}>
        <div class="rounded-md bg-[var(--color-bg)] p-3 space-y-2">
          <Show when={props.worktree.kind !== "review"} fallback={<><p class={heading}>Se publicaría así</p><p class="text-base leading-relaxed">{e().comment_en}</p><p class="text-base leading-relaxed text-[var(--color-fg-muted)]">Qué dice: {e().comment_es}</p></>}><p class={heading}>Cambio sugerido</p><p class="text-base leading-relaxed">{e().fix_es}</p></Show>
        </div>
      </Show>
    </article>;
  }
  return <section class="h-full flex flex-col min-h-0 bg-[var(--color-bg)] text-[var(--color-fg)] select-text text-base leading-relaxed [--text-base:16px] [--text-lg:18px]" aria-label="Revisar el PR">
    <div class="flex shrink-0 flex-wrap items-center gap-3 px-4 py-2 border-b border-[var(--color-border)] text-[13px] text-[var(--color-fg-muted)]">
      <Show when={prUrl()} fallback={<span>Sin PR todavía</span>}>{url => <a href={url()} class="underline underline-offset-2 hover:text-[var(--color-fg)]" onClick={event => { event.preventDefault(); void openUrl(url()).catch(e => setError(String(e))); }}>{prLabel()}</a>}</Show>
      <Show when={review()}>{r => <>
        <span class="font-mono">{r().target.head_sha.slice(0, 7)}</span><span>{r().usage.model}</span>
        <span title={`Entrada: ${r().usage.input_tokens} · Salida: ${r().usage.output_tokens} · Caché: ${r().usage.cache_read_tokens}`}>{(r().usage.input_tokens + r().usage.output_tokens + r().usage.cache_read_tokens).toLocaleString("es-AR")} tokens</span>
        <span>{money(r().usage.cost_usd)} · última vez: {money(view()?.last_cost_usd ?? 0)}</span>
        <Show when={view()?.cached}><span class="text-[var(--color-success)]">Volver a abrir: 0</span></Show>
      </>}</Show>
      <Show when={stale()}><span class="text-[var(--color-warn)]">Hay commits nuevos. Pedí que la actualicen.</span></Show>
      <Show when={!review() && (view()?.last_cost_usd ?? 0) > 0}><span>Último intento: {money(view()!.last_cost_usd)}</span></Show>
      <Show when={view()?.deep_available}><div class="ml-auto flex gap-4" role="tablist" aria-label="Tipo de revisión"><button role="tab" class="py-1 border-b border-transparent" classList={{"border-[var(--color-accent)]! text-[var(--color-fg)]!":review()?.kind !== "deep"}} aria-selected={review()?.kind !== "deep"} onClick={() => void load(false)}>Rápida</button><button role="tab" class="py-1 border-b border-transparent" classList={{"border-[var(--color-accent)]! text-[var(--color-fg)]!":review()?.kind === "deep"}} aria-selected={review()?.kind === "deep"} onClick={() => void load(true)}>Profunda</button></div></Show>
    </div>
    <Show when={view()?.deep_error}><p role="alert" class="p-3 text-[var(--color-warn)]">La revisión profunda no se pudo leer: {view()?.deep_error}</p></Show>
    <Show when={error() || view()?.error}><p role="alert" class="p-3 text-[var(--color-warn)] border-b border-[var(--color-border)]">{error() || view()?.error}</p></Show>
    <Show when={review()} fallback={<div class="flex-1 flex flex-col gap-4 items-center justify-center p-8 text-center text-[var(--color-fg-muted)]">
      <p>{loading() ? "Cargando la revisión…" : "Pedile al agente o al orquestador: «mostrame explicado este PR»"}</p>
    </div>}>
      <div class="flex flex-1 min-h-0">
        <nav class="w-[280px] shrink-0 overflow-auto border-r border-[var(--color-border)] p-3 space-y-2" aria-label="Archivos del PR">
          <p class={`${heading} px-2 pt-2 pb-1`}>Resumen</p>
          <button class="w-full text-left p-2 rounded-md hover:bg-[var(--color-bg-hover)]" classList={{ "bg-[var(--color-bg-hover)]": selected() === null }} aria-current={selected() === null ? "page" : undefined} onClick={() => setSelected(null)}>
            <span class="flex items-center justify-between text-lg font-semibold">El PR completo <span class="text-[var(--color-warn)] text-[13px]">{issues().length}</span></span>
            <span class="block text-[13px] text-[var(--color-fg-dim)] mt-1">{review()!.files.length} {review()!.files.length === 1 ? "archivo" : "archivos"}</span>
          </button>
          <p class={`${heading} px-2 pt-4 pb-1`}>Archivos</p>
          <For each={review()!.files}>{f => <button class="w-full text-left p-2 rounded-md hover:bg-[var(--color-bg-hover)]" classList={{ "bg-[var(--color-bg-hover)]": selected() === f.path }} aria-current={selected() === f.path ? "page" : undefined} onClick={() => { setSelected(f.path); setHighlight(null); }}>
            <span class="flex gap-2 justify-between text-lg font-semibold"><span>{f.name_es}</span><span class="text-[13px] text-[var(--color-warn)] shrink-0">{f.skipped ? "—" : f.evals.filter(e => e.severity !== "ok").length || "✓"}</span></span>
            <span class="block text-[13px] break-all text-[var(--color-fg-dim)] mt-1 font-mono">{f.path}</span>
          </button>}</For>
        </nav>
        <Show when={file()} fallback={<div class="grid grid-cols-2 flex-1 min-w-0 min-h-0">
          <div class={column}><h2 class={heading}>Qué hace el PR</h2><p class="text-base leading-relaxed whitespace-pre-wrap">{review()!.summary.what_es}</p>
            <For each={review()!.summary.flows}>{flow => <div class="space-y-2"><h3 class="text-lg font-semibold text-[var(--color-fg-muted)]">{flow.who_es}</h3>
              <div class="flex flex-wrap items-center gap-1.5"><For each={flow.steps}>{(step, i) => <><Show when={i() > 0}><span class="text-[var(--color-fg-dim)]">→</span></Show><span class="px-2 py-1 rounded-md border border-[var(--color-border)] text-[13px]" classList={{ "text-[var(--color-success)] bg-[var(--color-success)]/10 border-[var(--color-success)]!": step.changed }}>{step.label_es}</span></>}</For></div>
            </div>}</For><p class="text-[13px] text-[var(--color-fg-dim)]">En verde, lo que este PR agrega o cambia.</p>
          </div>
          <div class={`${column} border-l border-[var(--color-border)]`}><h2 class={heading}>Evaluación</h2><p class="rounded-lg bg-[var(--color-accent)]/10 p-3 text-base leading-relaxed">{review()!.summary.verdict_es}</p>
            <For each={issues()}>{({ f, e }) => <EvalCard file={f} eval={e} showFile />}</For>
          </div>
        </div>}>
          {f => <Show when={!f().skipped} fallback={<div class={`${column} flex-1`}><h2 class="text-lg font-semibold">{f().path}</h2><p class="text-[var(--color-fg-muted)]">No se explica: {f().skip_reason_es}.</p></div>}>
            <div class="grid grid-cols-2 flex-1 min-w-0 min-h-0">
              <div class={column}><h2 class={heading}>Código</h2><p class="font-mono text-[13px] text-[var(--color-fg-dim)] break-all">{f().path}</p>
                <div class="overflow-auto border border-[var(--color-border)] rounded-md py-2 bg-black font-mono text-[14px]">
                  <For each={diff()?.lines}>{line => {
                    const marked = () => f().evals.filter(e => e.severity !== "ok" && (e.side === "RIGHT" ? line.right : line.left) === e.start);
                    const lit = () => { const h = highlight(); const n = h?.side === "LEFT" ? line.left : line.right; return !!h && n != null && n >= h.start && n <= h.end; };
                    return <div class="flex w-full min-w-max text-left leading-[24px] px-2" classList={{ "bg-[var(--color-success)]/8": line.kind === "add", "bg-[var(--color-danger)]/10": line.kind === "delete", "text-[var(--color-accent)]": line.kind === "hunk", "italic text-[var(--color-fg-muted)]": /^\s*(#|\/\/|\/\*|\*|<!--)/.test(line.text), "bg-[var(--color-accent)]/25!": lit() }} aria-label={`Línea ${line.right ?? line.left ?? "del bloque"}`}>
                      <span class="min-w-6 pr-1 shrink-0 text-[13px]" classList={{ "text-[var(--color-warn)]": marked().some(e=>e.severity === "warn"), "text-[var(--color-accent)]": marked().every(e=>e.severity === "info") }}>{marked().map(e=>number(f(),e)).join(",")}</span>
                      <span class="w-9 shrink-0 text-right pr-2 text-[var(--color-fg-dim)]">{line.left}</span><span class="w-9 shrink-0 text-right pr-2 text-[var(--color-fg-dim)]">{line.right}</span>
                      <span class="w-4 shrink-0 text-[var(--color-fg-dim)]">{line.kind === "add" ? "+" : line.kind === "delete" ? "−" : ""}</span><span class="whitespace-pre">{line.text || " "}</span>
                    </div>;
                  }}</For>
                </div>
              </div>
              <div class={`${column} border-l border-[var(--color-border)]`}><h2 class={heading}>Qué hace</h2><p class="leading-relaxed text-base">{f().role_es}</p>
                <For each={f().parts}>{part => <article tabindex="0" class="p-4 rounded-lg hover:bg-[var(--color-bg-hover)] space-y-2" onMouseEnter={() => setHighlight(part)} onMouseLeave={() => setHighlight(null)} onFocusIn={() => setHighlight(part)} onFocusOut={() => setHighlight(null)}>
                  <h3 class="text-lg font-semibold">{part.name_es}</h3><p class="text-base leading-relaxed">{part.what_es}</p><p class="text-[13px] text-[var(--color-fg-dim)]">En el código: <code>{part.code_name}</code></p>
                </article>}</For>
                <h2 class={heading}>Evaluación</h2><For each={f().evals}>{e => <EvalCard file={f()} eval={e} />}</For>
              </div>
            </div>
          </Show>}
        </Show>
      </div>
    </Show>
  </section>;
}
