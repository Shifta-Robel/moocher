// Service worker: Web Push notifications for the dashboard (works with the tab closed).
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  let n = { title: "opencode-cluster", body: "", severity: "info", target: null, kind: "" };
  try { n = Object.assign(n, event.data ? event.data.json() : {}); } catch (e) { n.body = event.data ? event.data.text() : ""; }
  const url = targetUrl(n);
  event.waitUntil(
    self.registration.showNotification(n.title, {
      body: n.body,
      tag: n.kind + ":" + (n.target || ""),
      renotify: n.severity === "vital",
      requireInteraction: n.severity === "vital",
      icon: "/icon-192.png",
      badge: "/icon-192.png",
      data: { url },
    }),
  );
});

function targetUrl(n) {
  const t = n.target || "";
  if (t.startsWith("q-")) return "/questions";
  if (t.startsWith("h-") || t.startsWith("task:")) return "/tasks";
  if (t.startsWith("s-")) return "/instances/" + t;
  if (t.startsWith("node:")) return "/nodes";
  return "/notifications";
}

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const url = (event.notification.data && event.notification.data.url) || "/";
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((list) => {
      for (const c of list) {
        if ("focus" in c) { c.navigate(url); return c.focus(); }
      }
      return self.clients.openWindow(url);
    }),
  );
});
