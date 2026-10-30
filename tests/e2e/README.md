# End-to-end tests

Each suite starts a real hub (supervisor and its own node), enrolls "remote"
nodes over mTLS on 127.0.0.1, and drives them with the CLI against the scripted
mock LLM (`examples/mock-llm`). Sessions use the `process` runtime, so Podman is
not needed; container isolation itself is covered by the Phase 1 demo.

| Suite | What it proves |
|---|---|
| `enroll.sh` | Join tokens are single use and pin the CA. Pending nodes wait for approval. A session runs on a remote node. Relay nodes need an allowed source. Certificates renew. Revocation disconnects. |
| `relay.sh` | `via_node` routes: the provider sees the relay's IP and the key is still injected on the calling node. Relay port limits apply, and a session recovers when the relay comes back. |
| `quota.sh` | Per-IP allowances, failover to another IP mid-turn, shared-IP view, placement away from exhausted IPs, parking with a countdown and resuming, operator overrides, live route changes. |
| `templates.sh` | Hub template library, secret aliases, `overridable` caps, `instances.max`, the rendered preamble, account-scoped usage, library updates, `hub_templates: false`. |
| `stats.sh` | Host and instance stats, per-session history, the soft disk limit. |
| `resilience.sh` | The supervisor restarts while a remote node is working: the node buffers, reconnects and replays, and nothing is lost. |
| `artifacts.sh` | Operator and agent artifacts (files and trees, content addressing, links never packed), lineage, a shared workspace on two machines (sync before start and after turns, three-way merge, a conflict kept as `.theirs`), a workflow whose review step runs on another machine. |
| `questions.sh` | `cluster_ask` and OpenCode's `question` tool reaching the operator, a parent answering, escalation with a note, the four modes, rules raising importance, operator-only questions, timeouts, the assumption log, permissions and budget stops as questions. |
| `death.sh` | A killed instance detected (`exit:137`) and restarted in place with its history; a tree halted when recovery is not cheap enough, then resumed with one command; resume on another machine from the dead volume; a lost node resumed from its checkpoint; the wall-clock limit. |
| `web.sh` | HTTPS from certificate files (private CA), login, CSRF, scopes, lockouts, TOTP, the UI behind the login and its security headers, Open in OpenCode over TLS (`attach-check.mjs`: app, API, per-request cookie checks, terminal over WebSocket), `moo attach`, notifications to ntfy, a webhook (HMAC) and Web Push (`push-check.mjs` verifies VAPID and decrypts), promoting a route to YAML. |
| `acme.sh` | The hub gets its certificate from Pebble (Let's Encrypt's test CA) over HTTP-01, serves with it, redirects HTTP, reuses it after a restart. Needs `PEBBLE_DIR`. |
| `ui.sh` | The dashboard in headless Chromium (`ui-check.mjs`, Playwright): login, overview, New instance, live updates, preamble, messages, Open in OpenCode, answering a question, the question mode, the task board, Add machine, dark mode, the phone layout, logout, no JavaScript errors. Needs `PW_DIR`. Screenshots in `$MOO_E2E_DIR/shots`. |
| `harvest.sh` | Outputs collected on stop (files, a glob as one tree, a missing one reported), delivered into the parent's workspace and the task workspace, the transcript as an artifact, `retain:` never vs. the default. |
| `metering.sh` | keyproxy's own usage meter: it agrees with OpenCode on a normal turn and asks streams for usage; calls made around OpenCode with the capability token are counted, raise `usage.mismatch`, and run into the budget; the count survives a node restart. |
| `egress.sh` | `egress: allowlist`: the agent's shell reaches a listed host through the node proxy (plain HTTP and CONNECT); unlisted hosts and a listed name resolving to loopback are refused and recorded; live allowlist edits; an allowlist without hosts is rejected. |
| `ops.sh` | Daily event export (incremental zstd frames, idempotent), pruning only exported events, artifact expiry sparing tagged and referenced artifacts (dry run first), audit; `disk_enforce: hard` refusing to start where quotas cannot work. |
| `load.sh` | Load and chaos: 12 instances over 3 nodes against a too-small shared allowance (parking, bounded 429s, latency and memory); a worker killed with 4 instances mid-task (each resumed elsewhere or halted, nothing placed on the dead node); the supervisor killed under load (all finish, no duplicate events, no false `node_lost`, audit intact). ~6 minutes. |

## Running

The `acme` and `ui` suites run only when their tools are given:
- `PEBBLE_DIR`: a directory with Pebble v2.8 (`pebble`, `localhost-cert.pem`, `localhost-key.pem`, `pebble.minica.pem` from its `test/certs`);
- `PW_DIR`: a directory with `npm install playwright` and `npx playwright install chromium-headless-shell` (Chromium's system libraries must be present).

The `web` suite needs `openssl` and Node 22.


```bash
cargo build --workspace
npm install -g opencode-ai@1.19.4        # or set MOO_OPENCODE_BIN
tests/e2e/run-all.sh                      # or one suite: tests/e2e/quota.sh
```

Needs `python3`, and free ports 7700, 7443 and 8099. Scratch files go to
`MOO_E2E_DIR` (default `/tmp/moo-e2e`); keep that path short, because session
sockets must fit in 107 bytes. The relay tests use the loopback address
127.0.0.2 as the relay's egress IP (`egress_bind`), which Linux provides.
Each suite takes 1–3 minutes (the Phase 3 suites 3–8). Scripted models for a suite are written to `$MOO_E2E_DIR/work/scripts` with `mock_script` (see `lib.sh`).
