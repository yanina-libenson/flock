// Flock PWA — phase 3a: live worktree status list.
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

function render(worktrees) {
  const main = document.getElementById("main");
  const waiting = worktrees.filter((w) => w.status === "needs_input").length;
  document.getElementById("count").textContent = waiting
    ? `${waiting} waiting`
    : "";

  if (worktrees.length === 0) {
    main.innerHTML = `<div class="empty">No worktrees yet.<br/>Create one in Flock on your Mac.</div>`;
    return;
  }

  // Group by repo, preserving server order.
  const groups = [];
  const byRepo = new Map();
  for (const w of worktrees) {
    if (!byRepo.has(w.repo)) {
      byRepo.set(w.repo, []);
      groups.push(w.repo);
    }
    byRepo.get(w.repo).push(w);
  }

  const esc = (s) =>
    String(s).replace(
      /[&<>"]/g,
      (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c],
    );

  main.innerHTML = groups
    .map((repo) => {
      const rows = byRepo
        .get(repo)
        .map((w) => {
          const status = w.status || "";
          const title = w.title && w.title.trim() ? w.title : w.branch;
          const sub = w.title && w.title.trim() ? `<div class="branch">${esc(w.branch)}</div>` : "";
          return `<div class="wt ${status}">
            <span class="status ${status}" title="${STATUS_LABEL[status] || ""}"></span>
            <div class="body"><div class="title">${esc(title)}</div>${sub}</div>
          </div>`;
        })
        .join("");
      return `<div class="repo">${esc(repo)}</div>${rows}`;
    })
    .join("");
}

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

let timer = null;
async function tick() {
  if (!token) {
    showTokenPrompt();
    return;
  }
  try {
    const data = await api("/api/worktrees");
    render(data);
  } catch (e) {
    if (String(e).includes("unauthorized")) {
      showTokenPrompt();
      return;
    }
    // Network/daemon unreachable — keep the last render, retry quietly.
    const loading = document.getElementById("loading");
    if (loading) loading.textContent = "Can't reach Flock. Retrying…";
  }
}

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

tick();
timer = setInterval(tick, 2000);
