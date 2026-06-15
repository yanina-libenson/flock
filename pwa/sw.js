// Bump this whenever any shell asset (index.html, app.js, manifest) changes,
// or installed PWAs keep serving the stale shell forever.
const SW_VERSION = "flock-shell-v7";
const SHELL = ["/", "/app.js", "/manifest.webmanifest"];

self.addEventListener("install", (e) => {
  self.skipWaiting();
  e.waitUntil(caches.open(SW_VERSION).then((c) => c.addAll(SHELL)));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(keys.filter((k) => k !== SW_VERSION).map((k) => caches.delete(k))),
      )
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  // API calls are always live — never cache agent state.
  if (url.pathname.startsWith("/api/")) return;
  // Shell: network-first (updates land on plain reload), cache as offline
  // fallback.
  e.respondWith(
    fetch(e.request)
      .then((res) => {
        const copy = res.clone();
        caches
          .open(SW_VERSION)
          .then((c) => c.put(e.request, copy))
          .catch(() => {});
        return res;
      })
      .catch(() => caches.match(e.request)),
  );
});

self.addEventListener("push", (e) => {
  let data = {};
  try {
    data = e.data ? e.data.json() : {};
  } catch {
    /* non-JSON payload */
  }
  e.waitUntil(
    self.registration.showNotification(data.title || "Flock", {
      body: data.body || "",
      tag: "flock-needs-input",
      renotify: true,
    }),
  );
});

self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  e.waitUntil(
    self.clients.matchAll({ type: "window" }).then((wins) => {
      for (const w of wins) {
        if ("focus" in w) return w.focus();
      }
      return self.clients.openWindow("/");
    }),
  );
});
