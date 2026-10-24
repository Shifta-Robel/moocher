// Plays browser + push service for Web Push: subscribes with fresh keys, waits
// for the hub's push, checks the VAPID JWT (RFC 8292) and decrypts the
// aes128gcm body (RFC 8291). Usage: node push-check.mjs <hub url> <bearer token> <port>
import http from "node:http";
import crypto from "node:crypto";
const [hub, token, port] = process.argv.slice(2);
const b64u = (b) => Buffer.from(b).toString("base64url");
const ecdh = crypto.createECDH("prime256v1");
const uaPublic = ecdh.generateKeys();
const authSecret = crypto.randomBytes(16);
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? "ok" : "FAIL"} ${what}`); if (!ok) failed++; };
const api = (p, body) => fetch(hub + "/api/v1" + p, { method: body ? "POST" : "GET", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: body && JSON.stringify(body) }).then((r) => r.json());

const received = new Promise((resolve) => {
  const srv = http.createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => { res.writeHead(201); res.end(); srv.close(); resolve({ headers: req.headers, body: Buffer.concat(chunks) }); });
  }).listen(Number(port), "127.0.0.1");
});
const endpoint = `http://127.0.0.1:${port}/push/abc`;
const sub = await api("/push/subscribe", { endpoint, keys: { p256dh: b64u(uaPublic), auth: b64u(authSecret) } });
check(sub.ok === true, "subscription stored");
const { public_key } = await api("/push/key");
await api("/notifications/test", {});
const got = await Promise.race([received, new Promise((r) => setTimeout(() => r(null), 15000))]);
check(!!got, "the hub pushed to the subscription");
if (got) {
  const h = got.headers;
  check(h["content-encoding"] === "aes128gcm" && h["ttl"], "aes128gcm body with a TTL");
  const m = /vapid t=([^,]+), k=(.+)/.exec(h.authorization || "");
  check(!!m && m[2] === public_key, "VAPID header carries the hub's public key");
  if (m) {
    const [hd, cl, sig] = m[1].split(".");
    const key = crypto.createPublicKey({ key: { kty: "EC", crv: "P-256", x: b64u(Buffer.from(m[2], "base64url").subarray(1, 33)), y: b64u(Buffer.from(m[2], "base64url").subarray(33)) }, format: "jwk" });
    const ok = crypto.verify("sha256", Buffer.from(`${hd}.${cl}`), { key, dsaEncoding: "ieee-p1363" }, Buffer.from(sig, "base64url"));
    const claims = JSON.parse(Buffer.from(cl, "base64url"));
    check(ok && claims.aud === `http://127.0.0.1:${port}` && claims.exp > Date.now() / 1000, "VAPID JWT signature, audience and expiry valid");
  }
  // RFC 8291 decryption.
  const b = got.body;
  const salt = b.subarray(0, 16), idlen = b[20], asPublic = b.subarray(21, 21 + idlen), ct = b.subarray(21 + idlen);
  const shared = ecdh.computeSecret(asPublic);
  const hkdf = (s, ikm, info, n) => Buffer.from(crypto.hkdfSync("sha256", ikm, s, info, n));
  const ikm = hkdf(authSecret, shared, Buffer.concat([Buffer.from("WebPush: info\0"), uaPublic, asPublic]), 32);
  const cek = hkdf(salt, ikm, Buffer.from("Content-Encoding: aes128gcm\0"), 16);
  const nonce = hkdf(salt, ikm, Buffer.from("Content-Encoding: nonce\0"), 12);
  let plain = "";
  try {
    const d = crypto.createDecipheriv("aes-128-gcm", cek, nonce);
    d.setAuthTag(ct.subarray(ct.length - 16));
    const p = Buffer.concat([d.update(ct.subarray(0, ct.length - 16)), d.final()]);
    plain = p.subarray(0, p.lastIndexOf(2)).toString();
  } catch (e) { plain = "decrypt error " + e.message; }
  let msg = {};
  try { msg = JSON.parse(plain); } catch {}
  check(msg.title === "Test notification" && msg.severity === "warning", `decrypted payload: ${plain.slice(0, 80)}`);
}
process.exit(failed ? 1 : 0);
