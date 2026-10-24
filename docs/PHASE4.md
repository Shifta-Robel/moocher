# Phase 4 report: the dashboard, login and HTTPS, Open in OpenCode, notifications, TUI

Status: **done and verified end to end** on 2026-10-24, with OpenCode 1.19.4, the scripted mock LLM, the `process` runtime, headless Chromium (Playwright 1.59) and Pebble 2.8 as the ACME CA. Podman was not available on this test machine (see "Not verified here").

Design: [PHASE1_5.md](./PHASE1_5.md) §11, §13. Usage: [USAGE.md](./USAGE.md) §23–27. Operations: [RUNBOOK.md](./RUNBOOK.md) §7. Config keys: [CONFIG.md](./CONFIG.md).

## What was built

| Piece | Where | Notes |
|---|---|---|
| Login | `supervisor/src/auth.rs` | Argon2id passwords with `read/operate/admin` scopes. Session cookie (only its SHA-256 is stored) and a CSRF token. Optional TOTP (RFC 6238, each code accepted once). Lockouts per account and per IP, doubling to 24 h, with vital notifications. All audited. `ocl-supervisor user set/list/rm/totp`. |
| HTTPS | `supervisor/src/web.rs` | rustls from files (reloaded when replaced) or ACME HTTP-01 (instant-acme, any RFC 8555 CA) with renewal 30 days ahead, swapped in without a restart. Port-80 responder that redirects to HTTPS. `trusted_proxies` for running behind a reverse proxy. |
| Dashboard | `ui/web/`, embedded from `supervisor/ui-dist` | SolidJS + Tailwind v4 + Kobalte, every view in PHASE1_5 §13c, live over SSE. Statuses always carry colour, an icon and a word. Stable colour per node. Dark/light, phone layout, PWA. Strict CSP. ~85 KB gzipped in total; the first screen loads 43 KB of JS and 7.6 KB of CSS. |
| Open in OpenCode | `supervisor/src/attach.rs`, `node-daemon/src/attach.rs` | A second origin (`attach_listen`). One-time ticket → 8 h cookie. Each browser connection is spliced through a node tunnel to an HTTP proxy on the node, which checks the cookie hash on every request, adds the container's OpenCode password and passes WebSocket upgrades. `ocl attach` gives the same in a terminal. |
| Notifications | `supervisor/src/notify.rs` | Rules by kind and severity, quiet hours (vital ignores them). Web Push implemented from RFC 8291 (aes128gcm) and RFC 8292 (VAPID ES256). ntfy, and an HMAC-signed webhook. New sources: task and run outcomes, node offline/back, quota exhausted/reset, budget at 80%, provider auth failed / down, disk at 90%, lockouts, ACME failures. |
| Live editing | node `PromoteRoute`, dashboard | Routes, quota overrides and the allowlist from the UI. **Write to YAML** turns a live route override into the agent file's `route:` (validated before writing). |
| TUI | `cli/src/tui.rs` | `ocl tui`: instances, questions, halts and tasks, nodes. Answer, message, stop and recover from the keyboard. |
| E2E | `tests/e2e/{web,acme,ui}.sh` + `attach-check.mjs`, `push-check.mjs`, `ui-check.mjs` | Three new suites. The push and attach checks are independent clients written in Node: they verify signatures, decrypt pushes and speak WebSocket. |

## Verified (each was run, not just compiled)

All thirteen suites passed (`tests/e2e/run-all.sh` with `PEBBLE_DIR` and `PW_DIR` set), **266 checks**. The browser suite's "every browser check passed" stands for 17 steps in Chromium.

| Suite | Checks | | Suite | Checks |
|---|---|---|---|---|
| enroll | 29 | | questions | 32 |
| relay | 14 | | death | 38 |
| quota | 24 | | harvest | 17 |
| templates | 19 | | **web** | **38** |
| stats | 8 | | **acme** | **10** |
| resilience | 9 | | **ui** | **3** (17 browser steps) |
| artifacts | 25 | | | |

In the full run, `quota` failed once on a timing race in its own check: it slept 4 s before reading the countdown of a session that had resumed by then. The check now reads state and countdown together; the suite then passed on its own (24/24).

Unit tests: 39.

| Requirement | Evidence |
|---|---|
| Password login on your own domain | Pages redirect to `/login?next=…`. A wrong password is refused. Login sets an `HttpOnly; Secure; SameSite=Strict` session. Logout ends it. |
| CSRF and scopes | A change from the browser without `X-CSRF-Token` gets 403, and 200 with it. An `operate` user cannot list users (403). |
| Rate limiting and lockout notifications | Three failures (`max_failures: 3` in the test) lock the account, even against the right password (429). The lockout is a vital notification and is in the audit log. |
| TOTP | With TOTP on, the password alone asks for a code. Password plus code logs in, and the same code is refused the second time. |
| HTTPS from your own certificate | The API, CLI (`OCL_CA_CERT`), dashboard and attach port all serve the configured certificate; plain HTTP on that port gets nothing. |
| HTTPS from Let's Encrypt (ACME) | Against Pebble, the hub registered, answered the HTTP-01 challenge (Pebble validated it), finalized with its own key, and served the issued certificate (`issuer: Pebble`, `DNS:localhost`). Keys are 0600, the HTTP port redirects to HTTPS, the issuance is audited, and a restart reuses the cached certificate without a new order. |
| Open in OpenCode | Over TLS: the ticket gave an HttpOnly cookie and works only once. OpenCode's web app and assets loaded, and its API answered (the node added the password). Without the cookie, or with a forged one, the result is 401, **also on a kept-alive connection**. A terminal over WebSocket echoed back through the hub. The instance shows *attached by*. `ocl attach` served OpenCode locally with no password on the client side. |
| Notifications to the phone (Web Push) | An independent client subscribed with fresh P-256 keys. The hub's push carried `aes128gcm`, a TTL and a VAPID header whose ES256 JWT verified (audience = the push origin, valid expiry). The body decrypted to the expected notification. The unit test reproduces RFC 8291's published example byte for byte. |
| ntfy and webhook | ntfy got title, priority and the bearer token. The webhook got JSON whose `X-OCL-Signature` matched the HMAC. The vital lockout went out too; info-level events stayed in-app. |
| UI editing through overrides.yaml, promote to YAML | A live route override was written to `agents/builder.yaml` as `route:` and removed from `overrides.yaml`; the agent still loads. |
| The dashboard in a real browser | Headless Chromium went through these steps, with no JavaScript errors:<br>1. Login, including a wrong password and the redirect back to the page asked for.<br>2. Overview with the live status strip.<br>3. **New instance** from Templates, then the instance page updating live to idle with 3/3 todos, the preamble tab and a queued message.<br>4. **Open in OpenCode** opened OpenCode's app in a new tab.<br>5. A question from an agent appeared with `blocks 1` and was answered with one click; the answer reached the agent.<br>6. Switched the question mode.<br>7. Added two tasks and reordered them.<br>8. **Add machine** showed the join command.<br>9. Dark mode, remembered.<br>10. The phone layout (390 px: bottom tab bar, no sidebar, no sideways scrolling).<br>11. Logout. |
| TUI | Unit-tested by rendering into a test terminal: states with symbols, todo progress, question badges and the detail view. |
| Phases 1–3 | The ten earlier suites pass unchanged in the same run. |

The browser run found three bugs, all fixed:
- an input `pattern` that modern browsers reject (the `v` regex flag);
- a dialog that ignored Escape once its focused button was replaced (focus now moves into the dialog);
- a live question showing `blocks 0` (the count now goes with the event too).

The attach checks also found that only the first request of a connection was checked; checks are now per request, at the node.

Unit tests added in this phase:
- login, lockout and TOTP;
- RFC 6238 vectors;
- the RFC 8291 vector;
- VAPID;
- notification rules and quiet hours;
- one-time attach tickets;
- TUI rendering.

## What the design changed while building

1. **The node checks the attach cookie on every request,** not only the hub on the first one. The hub hands the node the cookie's hash and expiry with the tunnel.
2. **The hub splices attach connections byte for byte** after checking the first request's cookie. HTTP parsing happens at the node, where the OpenCode password is added. WebSocket upgrades just work.
3. **Web Push is implemented directly from the RFCs** (p256, hkdf, aes-gcm) rather than through a crate needing OpenSSL. It is checked against the RFC's test vector and against an independent decryptor.
4. **The UI build is checked in** (`supervisor/ui-dist`), so building the Rust workspace needs no Node. A `build.rs` makes Cargo re-embed it when it changes.
5. **The CLI never follows redirects,** so `ocl attach` can take the cookie from the ticket's 302. `OCL_CA_CERT` trusts a private CA.
6. **"Add machine" asks for admin scope, not a fresh password** (the design said re-authentication). A browser session already proves the password, and scopes apply.
7. **Questions on the live stream carry their blocked count,** like the REST view.

## Not verified here / known limitations

| Item | Status |
|---|---|
| **Podman runtime** | Not run on this machine (the same as in Phases 2–3). Phase 4 adds nothing Podman-specific; the attach proxy talks to the same `oc.sock`. |
| Real Let's Encrypt and real browsers' push services | ACME was tested against Pebble (the same protocol) and Web Push against an RFC-conformant test client. Production Let's Encrypt and Google/Apple/Mozilla push endpoints were not contacted from here. |
| iOS | Web Push on iPhone needs the dashboard added to the home screen (Apple's rule); not tested on a device. |
| UI on real phones | The phone layout was checked in a 390 px mobile Chromium viewport, not on hardware. |
| Rolled-up todo board per task | Each instance shows its todos; a single board per task is not built. |
| Users' TOTP set-up | Only from the hub's command line (`ocl-supervisor user totp`), not from the dashboard. |
| OIDC / multi-user roles beyond the three scopes | Not built (as designed: one trusted operator). |

## Measured footprint

Release builds, idle and connected (`VmRSS`). Budgets from ARCHITECTURE §2: supervisor < 60 MB, node < 40 MB.

| Process | Phase 4 | Phase 3 |
|---|---|---|
| `ocl-supervisor` | 16.5 MB | 14.1 MB |
| `opencode-node` (hub's own / worker) | 15.4 / 15.7 MB | 15.7 / 15.7 MB |
| `opencode-node` (relay) | 13.5 MB | 13.9 MB |

Under load:
- a 64 MiB artifact put and get: supervisor 19.6 MB;
- serving the whole UI 20 times: 19.8 MB.

Binary sizes: supervisor 21.5 MB (the embedded UI, TLS/ACME and push add ~5 MB), node 22.0 MB, `ocl` 11.2 MB, `ocl-bridge` 1.2 MB.

Dashboard: ~85 KB gzipped in total. The first screen loads 42.6 KB of JS, 7.6 KB of CSS and a 0.5 KB HTML shell; every other page loads on demand (1–5 KB each).
