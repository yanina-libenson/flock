// Flock PWA — phases 3a/3b: live worktree status list + read-only terminal.
// Auth: token arrives once via ?token=… (paste from the desktop Settings),
// gets stashed in localStorage, and is stripped from the URL so it isn't
// left in history/screenshots.

const TOKEN_KEY = "flock.token";

function readToken() {
  const params = new URLSearchParams(location.search);
  const fromUrl = params.get("token");
  if (fromUrl) {
    localStorage.setItem(TOKEN_KEY, fromUrl);
    history.replaceState({}, "", location.pathname);
    return fromUrl;
  }
  return localStorage.getItem(TOKEN_KEY);
}

let token = readToken();
let currentWorktrees = [];

async function api(path) {
  const res = await fetch(path, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (res.status === 401) {
    localStorage.removeItem(TOKEN_KEY);
    token = null;
    throw new Error("unauthorized");
  }
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

const STATUS_LABEL = {
  working: "Working",
  idle: "Idle",
  needs_input: "Waiting for you",
};

const esc = (s) =>
  String(s).replace(
    /[&<>"]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c],
  );

// ---------- push notifications ----------

function pushSupported() {
  return (
    "serviceWorker" in navigator &&
    "PushManager" in window &&
    "Notification" in window
  );
}

function urlB64ToUint8Array(b64) {
  const pad = "=".repeat((4 - (b64.length % 4)) % 4);
  const base64 = (b64 + pad).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(base64);
  const arr = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) arr[i] = raw.charCodeAt(i);
  return arr;
}

async function enablePush() {
  try {
    const perm = await Notification.requestPermission();
    if (perm !== "granted") return;
    const reg = await navigator.serviceWorker.ready;
    const keyRes = await fetch("/api/push/vapid-public-key", {
      headers: { Authorization: `Bearer ${token}` },
    });
    const keyB64 = (await keyRes.text()).trim();
    const sub = await reg.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: urlB64ToUint8Array(keyB64),
    });
    await fetch("/api/push/subscribe", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(sub.toJSON()),
    });
    tick(); // refresh to drop the enable banner
  } catch (e) {
    console.error("enablePush failed", e);
    alert(`Couldn't enable notifications:\n${e}`);
  }
}

function pushBannerHtml() {
  if (!pushSupported() || Notification.permission === "granted") return "";
  return `<button id="enable-push" class="push-banner">🔔 Enable notifications on this device</button>`;
}

function wireBanner() {
  const eb = document.getElementById("enable-push");
  if (eb) eb.onclick = enablePush;
}

function render(worktrees) {
  currentWorktrees = worktrees;
  const main = document.getElementById("main");
  const waiting = worktrees.filter((w) => w.status === "needs_input").length;
  document.getElementById("count").textContent = waiting
    ? `${waiting} waiting`
    : "";

  if (worktrees.length === 0) {
    main.innerHTML =
      pushBannerHtml() +
      `<div class="empty">No worktrees yet.<br/>Create one in Flock on your Mac.</div>`;
    wireBanner();
    return;
  }

  const groups = [];
  const byRepo = new Map();
  for (const w of worktrees) {
    if (!byRepo.has(w.repo)) {
      byRepo.set(w.repo, []);
      groups.push(w.repo);
    }
    byRepo.get(w.repo).push(w);
  }

  const groupsHtml = groups
    .map((repo) => {
      const rows = byRepo
        .get(repo)
        .map((w) => {
          const status = w.status || "";
          const title = w.title && w.title.trim() ? w.title : w.branch;
          const sub =
            w.title && w.title.trim()
              ? `<div class="branch">${esc(w.branch)}</div>`
              : "";
          return `<div class="wt ${status}" data-id="${w.id}">
            <span class="status ${status}" title="${STATUS_LABEL[status] || ""}"></span>
            <div class="body"><div class="title">${esc(title)}</div>${sub}</div>
            <span class="chev">›</span>
          </div>`;
        })
        .join("");
      return `<div class="repo">${esc(repo)}</div>${rows}`;
    })
    .join("");
  main.innerHTML = pushBannerHtml() + groupsHtml;
  wireBanner();
}

// Delegated tap → open terminal.
document.getElementById("main").addEventListener("click", (e) => {
  const row = e.target.closest(".wt");
  if (!row) return;
  const id = Number(row.getAttribute("data-id"));
  const w = currentWorktrees.find((x) => x.id === id);
  if (w) openTerm(w);
});

// ---------- new task ----------

document.getElementById("new-task-btn").onclick = openNewTask;

async function openNewTask() {
  let repos = [];
  try {
    repos = await api("/api/repos");
  } catch (e) {
    console.error("repos fetch failed", e);
  }
  const overlay = document.createElement("div");
  overlay.className = "form-screen";
  overlay.innerHTML = `
    <div class="term-header">
      <button class="back">‹ Cancel</button>
      <div class="t">New task</div>
    </div>
    <div class="form-body">
      <div>
        <label>Repo</label>
        <select id="nt-repo">${repos.map((r) => `<option>${esc(r)}</option>`).join("")}</select>
      </div>
      <div>
        <label>Prompt</label>
        <textarea id="nt-prompt" placeholder="What should the agent do?" autofocus></textarea>
      </div>
      <div class="actions">
        <button class="cancel">Cancel</button>
        <button class="create">Create task</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  const close = () => overlay.remove();
  overlay.querySelector(".back").onclick = close;
  overlay.querySelector(".cancel").onclick = close;
  overlay.querySelector(".create").onclick = async () => {
    const repo = overlay.querySelector("#nt-repo").value;
    const prompt = overlay.querySelector("#nt-prompt").value.trim();
    if (!repo || !prompt) return;
    const btn = overlay.querySelector(".create");
    btn.textContent = "Creating…";
    btn.disabled = true;
    try {
      const res = await fetch("/api/tasks", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${token}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ repo, prompt }),
      });
      if (!res.ok) throw new Error(await res.text());
      close();
      tick();
    } catch (e) {
      console.error("create task failed", e);
      alert(`Couldn't create task:\n${e}`);
      btn.textContent = "Create task";
      btn.disabled = false;
    }
  };
}

// ---------- terminal view (read-only) ----------

let term = null;
let fit = null;
let es = null;
let overlay = null;
let onResize = null;

function b64ToBytes(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

async function sendInput(id, body) {
  try {
    await fetch(`/api/worktrees/${id}/input`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    });
  } catch (e) {
    console.error("sendInput failed", e);
  }
}

const VKEYS = [
  ["Esc", "escape"],
  ["Tab", "tab"],
  ["⇧Tab", "shift-tab"],
  ["↑", "up"],
  ["↓", "down"],
  ["←", "left"],
  ["→", "right"],
  ["Ctrl-C", "ctrl-c"],
];

function openTerm(w) {
  clearInterval(timer); // pause the list poll while viewing
  const title = w.title && w.title.trim() ? w.title : w.branch;

  overlay = document.createElement("div");
  overlay.className = "term-screen";
  overlay.innerHTML = `
    <div class="term-header">
      <button class="back">‹ Back</button>
      <div class="t">${esc(title)}</div>
    </div>
    <div class="term-host" id="term-host"></div>
    <div class="term-keys" id="term-keys"></div>
    <div class="term-compose">
      <input id="term-input" type="text" placeholder="Message…  (Send to submit)"
             autocomplete="off" autocapitalize="off" autocorrect="off" />
      <button id="term-send">Send</button>
    </div>`;
  document.body.appendChild(overlay);
  overlay.querySelector(".back").onclick = closeTerm;

  const keysEl = overlay.querySelector("#term-keys");
  for (const [label, key] of VKEYS) {
    const b = document.createElement("button");
    b.textContent = label;
    b.onclick = () => sendInput(w.id, { key });
    keysEl.appendChild(b);
  }

  const inputEl = overlay.querySelector("#term-input");
  const submit = async () => {
    const v = inputEl.value;
    if (!v) return;
    inputEl.value = "";
    await sendInput(w.id, { text: v });
    await sendInput(w.id, { key: "enter" });
  };
  overlay.querySelector("#term-send").onclick = submit;
  inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      submit();
    }
  });

  term = new Terminal({
    fontSize: 12,
    fontFamily: "ui-monospace, Menlo, monospace",
    cursorBlink: false,
    disableStdin: true,
    convertEol: false,
    theme: { background: "#0b0d10", foreground: "#e6e9ef" },
  });
  fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(document.getElementById("term-host"));
  fit.fit();

  onResize = () => {
    try {
      fit.fit();
    } catch {}
  };
  window.addEventListener("resize", onResize);
  window.addEventListener("orientationchange", onResize);

  es = new EventSource(
    `/api/worktrees/${w.id}/stream?token=${encodeURIComponent(token)}`,
  );
  es.onmessage = (ev) => {
    // Each frame is a full screen snapshot; clear then repaint.
    term.write("\x1b[2J\x1b[H");
    term.write(b64ToBytes(ev.data));
  };
  es.addEventListener("exit", () => {
    const note = document.getElementById("term-note");
    if (note) note.textContent = "Session ended.";
  });
}

function closeTerm() {
  if (es) {
    es.close();
    es = null;
  }
  if (onResize) {
    window.removeEventListener("resize", onResize);
    window.removeEventListener("orientationchange", onResize);
    onResize = null;
  }
  if (term) {
    term.dispose();
    term = null;
    fit = null;
  }
  if (overlay) {
    overlay.remove();
    overlay = null;
  }
  tick();
  timer = setInterval(tick, 2000);
}

// ---------- token prompt ----------

function showTokenPrompt() {
  const main = document.getElementById("main");
  document.getElementById("count").textContent = "";
  main.innerHTML = `
    <div class="token-prompt">
      <div>Paste your Flock access token.</div>
      <div style="font-size:12px;color:var(--fg-dim);margin-top:4px">Find it in Flock → Settings on your Mac.</div>
      <input id="token-input" type="password" placeholder="token" autocomplete="off" />
      <br/><button id="token-save">Connect</button>
    </div>`;
  document.getElementById("token-save").onclick = () => {
    const v = document.getElementById("token-input").value.trim();
    if (!v) return;
    localStorage.setItem(TOKEN_KEY, v);
    token = v;
    tick();
  };
}

// ---------- list polling ----------

let timer = null;
async function tick() {
  const newTaskBtn = document.getElementById("new-task-btn");
  if (!token) {
    if (newTaskBtn) newTaskBtn.hidden = true;
    showTokenPrompt();
    return;
  }
  if (newTaskBtn) newTaskBtn.hidden = false;
  try {
    const data = await api("/api/worktrees");
    render(data);
  } catch (e) {
    if (String(e).includes("unauthorized")) {
      showTokenPrompt();
      return;
    }
    const loading = document.getElementById("loading");
    if (loading) loading.textContent = "Can't reach Flock. Retrying…";
  }
}

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

tick();
timer = setInterval(tick, 2000);
