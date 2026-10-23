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

## Running

```bash
cargo build --workspace
npm install -g opencode-ai@1.19.4        # or set OCL_OPENCODE_BIN
tests/e2e/run-all.sh                      # or one suite: tests/e2e/quota.sh
```

Needs `python3`, and free ports 7700, 7443 and 8099. Scratch files go to
`OCL_E2E_DIR` (default `/tmp/ocl-e2e`); keep that path short, because session
sockets must fit in 107 bytes. The relay tests use the loopback address
127.0.0.2 as the relay's egress IP (`egress_bind`), which Linux provides.
Each suite takes 1–3 minutes.
