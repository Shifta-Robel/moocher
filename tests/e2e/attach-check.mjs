// Drives "Open in OpenCode" like a browser would: redeem the ticket, load
// OpenCode's web app and API through the hub, open a terminal over WebSocket.
// Usage: node attach-check.mjs <ticket url>   (prints one line per check; exit 1 on failure)
const url = process.argv[2];
const base = new URL(url).origin;
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? "ok" : "FAIL"} ${what}`); if (!ok) failed++; };

const r = await fetch(url, { redirect: "manual" });
const set = r.headers.get("set-cookie") || "";
const cookie = (set.match(/ocl_attach=[0-9a-f]+/) || [""])[0];
check(r.status === 302 && cookie && /HttpOnly/i.test(set), `ticket redeemed for an HttpOnly cookie (${r.status})`);
const H = { cookie };

const page = await fetch(base + "/", { headers: H });
const html = await page.text();
check(page.status === 200 && /<script[^>]+\/assets\//.test(html), `OpenCode's web app served through the hub (${page.status})`);
const asset = (html.match(/\/assets\/[^"]+\.js/) || [])[0];
const a = await fetch(base + asset, { headers: H });
check(a.status === 200 && (await a.arrayBuffer()).byteLength > 100000, `its assets load from the same origin (${asset})`);

const health = await fetch(base + "/api/health", { headers: H }).then((x) => x.json()).catch(() => ({}));
check(health.healthy === true, "OpenCode API answers (the node added its password)");
const sessions = await fetch(base + "/session", { headers: H }).then((x) => x.json()).catch(() => null);
check(Array.isArray(sessions) && sessions.length >= 1, `the instance's sessions are visible (${Array.isArray(sessions) ? sessions.length : "?"})`);

const noCookie = await fetch(base + "/api/health");
check(noCookie.status === 401, `without the cookie the hub refuses (${noCookie.status})`);
const forged = await fetch(base + "/api/health", { headers: { cookie: "ocl_attach=" + "0".repeat(64) } });
check(forged.status === 401, `a forged cookie is refused (${forged.status})`);
const pw = await fetch(base + "/api/health", { headers: { authorization: "Basic " + btoa("opencode:guess") } });
check(pw.status === 401, "basic auth alone gets nowhere (the hub checks its cookie first)");

// A terminal: create a PTY, then speak WebSocket to it through the tunnel.
const pty = await fetch(base + "/pty", { method: "POST", headers: { ...H, "content-type": "application/json" }, body: JSON.stringify({ command: "/bin/sh" }) }).then((x) => x.json()).catch((e) => ({ error: String(e) }));
check(typeof pty.id === "string", `PTY created (${pty.id ?? JSON.stringify(pty).slice(0, 80)})`);
if (pty.id) {
  const tok = await fetch(base + `/pty/${pty.id}/connect-token`, { method: "POST", headers: H }).then((x) => x.json()).catch(() => ({}));
  const q = tok.ticket ? `?ticket=${encodeURIComponent(tok.ticket)}` : "";
  const ws = new WebSocket(base.replace(/^http/, "ws") + `/pty/${pty.id}/connect${q}`, { headers: H });
  ws.binaryType = "arraybuffer";
  const out = await new Promise((resolve) => {
    let buf = "";
    const done = setTimeout(() => resolve(buf), 8000);
    ws.onopen = () => ws.send("echo attach-$((40+2))\n");
    ws.onmessage = (m) => {
      buf += typeof m.data === "string" ? m.data : new TextDecoder().decode(m.data);
      if (buf.includes("attach-42")) { clearTimeout(done); resolve(buf); }
    };
    ws.onerror = (e) => { clearTimeout(done); resolve("ERROR " + (e.message || e.type)); };
  });
  check(out.includes("attach-42"), "terminal over WebSocket works end to end (upgrade passed through)");
  ws.close();
}
process.exit(failed ? 1 : 0);
