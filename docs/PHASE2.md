# Phase 2 report: remote nodes, relays, quotas, placement, templates, stats

Status: **done and verified end to end** on 2026-10-23, with OpenCode 1.19.4, the scripted mock LLM and the `process` runtime. Podman was not available on this test machine (see "Not verified here").

Design: [PHASE1_5.md](./PHASE1_5.md). Operations: [RUNBOOK.md](./RUNBOOK.md). Config keys: [CONFIG.md](./CONFIG.md).

## What was built

| Piece | Where | Notes |
|---|---|---|
| Cluster CA, link TLS | `supervisor/src/{pki,tlslink}.rs`, `shared-protocol/src/tls.rs` | Ed25519 CA (rcgen), TLS 1.3 (rustls/ring). The link certificate covers `public_names` and is reissued when they change. The TLS listener puts peer address + client-cert fingerprint into every gRPC call. |
| Enrollment and admission | `supervisor/src/nodes.rs`, `node-daemon/src/pki.rs` | Join tokens `ocljoin.<secret>.<CA fingerprint>` (single use, 15 min, bound to name + role). CA pinning on first contact. CSR issuance (the key stays on the node). Approval, revocation, renewal at 2/3 of the lifetime. Source allowlist for relays (or all nodes). |
| Relay nodes and `via_node` | `node-daemon/src/relay.rs`, `supervisor/src/link.rs` (`Tunnel` RPC) | keyproxy → local SOCKS5 (password) → `Tunnel{Dial}` → hub → `OpenTunnel` → relay connects from its IP (or `egress_bind`) → `Tunnel{Accept}` → spliced. `relay_ports` / `relay_allow_hosts` limits. `role: relay` needs no Podman. |
| Egress IPs | `node-daemon/src/egress.rs` | Per route: the hub-observed address or an `ip_echo` probe through that route. One HTTP client per route, shared by all sessions. |
| Quotas and failover | `node-daemon/src/quota.rs`, `gateway.rs`, `supervisor/src/quotas.rs` | Buckets per provider × IP/key/account. Learned from rate-limit headers (OpenAI/Groq, Anthropic, OpenRouter), `retry-after`, Gemini `retryDelay`, and 429s; configured fixed/rolling windows with wall-clock resets; overrides. Persisted, merged across nodes. Route plans with failover; the new `parked` state. |
| Live overrides | `overrides.yaml`, `NodeCommand::{SetRoute,SetQuota}` | Route and quota changes from the CLI (UI in Phase 4), audited at the hub and kept out of versioned YAML. |
| Placement | `supervisor/src/placement.rs` | Score = capacity, quota headroom, provider latency, locality, link RTT (weights configurable). Exhausted nodes are skipped. The reason is stored per session. `cluster_list_nodes` tool. |
| Templates and preamble | `supervisor/src/templates.rs`, `node-daemon/src/{cfgstore,manager}.rs` | Hub `templates/` + `preambles/cluster.md` pushed to nodes and cached. Secret aliases via node `secrets:`. `instances.max`, `overridable`. Layered rendered preamble, readable per session. `hub_templates: false` opt-out. |
| Resource stats | `node-daemon/src/stats.rs`, `supervisor/src/resources.rs` | Host + instance CPU/memory/pids/disk every 10 s (cgroup v2 or `/proc`). An hour in memory, minute samples for 7 days. Soft `container.disk` limit, sized `/tmp`. |
| CLI | `cli/` | `ocl node invite/approve/revoke/forget/probe`, `ocl allow`, `ocl quotas`, `ocl ips`, `ocl quota set/clear`, `ocl route set/clear`, `ocl templates`, `ocl stats`, `ocl show --preamble`, spawn `--append/--budget/--route`. |
| E2E suites | `tests/e2e/` | Six suites, 109 checks, rerunnable on one machine. |

## Verified (each was run, not just compiled)

All six suites passed back to back (`tests/e2e/run-all.sh`). The Phase 1 demo (`run-local.sh --with-worker`) still passes: the workflow Passed and the task was done, with every session stopped at the end.

| Requirement | Evidence |
|---|---|
| Remote enrollment with a credential | A worker joins with a token; `node.key` is 0600 on the node. The node is `pending` until approved, then connects and runs a session; the hub records its source IP. |
| Tokens and CA pinning | A reused token is refused. A token with the wrong CA fingerprint is refused before anything is sent. |
| Relay allowlist (your request) | A relay join from a non-allowed IP is refused and allowed once `ocl allow add` covers it. Removing the entry disconnects the relay immediately and keeps it out. Workers are unaffected under `enforce_for: relay`. Spawning on a relay is refused. |
| Revocation and renewal | A revoked node is disconnected and cannot reconnect. Renewal (forced by backdating) issues a new certificate, and the node reconnects with it. |
| `via_node` egress | The provider saw **only** the relay's address (127.0.0.2 via `egress_bind`), and the real key was injected on the worker. The hub counted the relayed bytes. |
| Relay failure handling | With the relay refusing the port, the session showed `retrying (attempt 1, next in 2s): … relay-2: port 8099 is not in this relay's relay_ports [443]`. Once the port was allowed, it finished the turn by itself. |
| Usage per IP for keyless providers | `ocl quotas`: `mock ip 127.0.0.1 requests 4/4 EXHAUSTED resets in 34s (header)`. The hub's per-IP counts equal the provider's own counts. Tokens are attributed per IP. |
| Agents sharing an IP | `ocl ips` → `127.0.0.1 nodes hub,worker-b`. |
| Failover to another IP when the allowance runs out | Mid-turn the direct allowance (4 requests) ran out; the remaining requests went out via the relay (`{'127.0.0.1': 4, '127.0.0.2': 3}`). A `route.failover` event was recorded, and later `route.restored`. |
| Time left until reset, and setting it yourself | Reset countdowns are learned from headers. `ocl quota set mock --scope-id 127.0.0.2 --remaining 0 --resets-at +20s` was written to the node's `overrides.yaml`, and `ocl quota clear` removed it. |
| Parking | With both IPs exhausted the session was `parked` (`resets in 16s: waiting for the mock allowance on every route: …`). It resumed by itself after the reset and finished the queued request: the docstring was in the file. |
| Placement uses links and quotas | A new builder went to worker-b, not the hub, though the hub had more free slots: `skipped hub 0.59 (… quota 0.00) [every route's mock allowance is used up (resets in 34s)]`. Link RTT is shown for every node (3–5 ms locally). |
| Accounts behind API keys | The template's alias, mapped with `account: demo-account`, was counted as `account:demo-account` (7 requests, 19 146 tokens). |
| Templates on demand | `ocl templates` shows the template is runnable on worker-b only, and gives the hub's reason (`secret alias does not resolve on this node`). A budget above the cap and an unlisted route are refused. A second live instance is refused (`instances.max: 1`). The cached library contains no secret value. Edits reach the nodes within 10 s, and `hub_templates: false` drops them. |
| Preamble | The rendered instructions contain the hub preamble with the instance id, template, node, budget override, tools and the spawner's `role_append`, with no unrendered variables. |
| Container-level info and restrictions | `ocl stats`: per instance CPU % (e.g. 7.4), memory against its limit (234M/1024M), processes and disk. Host totals for every node. Per-session history (10 s and per-minute). An agent with `container.disk: 1m` was interrupted with `disk limit exceeded: 116.5 MiB used of 1.0 MiB`. |
| Supervisor restart with remote nodes | The supervisor was killed mid-turn for 15 s. The node buffered events, reconnected over mTLS and replayed them. The session ended `idle` with 3/3 todos and 24 138 tokens. Quota counts (7 calls) and the audit chain survived. |
| Phase 1 fix: stopped sessions stay stopped | Covered by a regression test (`late_report_does_not_revive_stopped_session`), and by the demo ending with every session `stopped`. |

Unit tests: 26, covering:
- the CA issuing verifiable certificates with the hub-chosen name;
- join token and URL parsing;
- every rate-limit header format, `retry-after` and Gemini `retryDelay`;
- wall-clock daily windows in a +02:00 zone;
- per-limit counters;
- overrides and persistence;
- override rules;
- cgroup parsing and `/proc` sampling;
- size and duration parsing;
- the documented example configs parsing.

## What the design changed while building

1. **Identity is the certificate fingerprint**, recorded at issuance, rather than a parsed subject. This needs no X.509 parsing on the hot path, and revocation and rotation are row updates.
2. **One TLS port for enrollment and links.** Client certificates are optional at the TLS layer and required per RPC. This avoids a second listener and a second certificate.
3. **Tunnels are separate gRPC streams**, not frames inside the JSON link, so a busy tunnel cannot delay commands. The relay dials back, so relays need no inbound ports.
4. **`via_node` uses a local SOCKS5 entry point**, so reqwest's proxy support carries both `https` and plain `http` providers unchanged.
5. **Waiting for a reset uses OpenCode's own retry.** keyproxy answers 429 with `retry-after`, and OpenCode's `retry` status (with `next`) does the waiting. The node shows `parked` with the countdown and re-prompts nothing.
6. **A 429 or an unreachable route triggers failover** before any byte reaches OpenCode. Failing over on errors, not just on quota, also covers a relay that is down.
7. **Tokens per bucket are attributed after the fact.** OpenCode's counters are per message, so tokens go to the bucket that served the request on the next request or at turn end. Requests are counted exactly.
8. **Resource samples are best effort**, outside the durable outbox. History is not worth replaying after an outage, and the outbox stays small.
9. `on_exhausted: fallback_provider` from the design was dropped for now (`wait` or `fail`). A failover plan over routes covers the per-IP case. A provider fallback needs a second model config and is noted for later.

## Not verified here / known limitations

| Item | Status |
|---|---|
| **Podman runtime** with Phase 2 code | Not run on this machine (no Podman or root). The keyproxy, relay and quota code paths are the same for both runtimes. Untested: the Podman-only pieces, which are the cgroup-v2 stats path (`podman inspect … CgroupPath`) and `--tmpfs size` from `container.tmp`. The Phase 1 container checks (no network, no key in the container, read-only config) were not repeated. **Run `examples/quickstart/run-local.sh` on a Podman host before relying on it.** |
| Real providers | Header parsing is unit-tested against the documented formats of OpenAI/Groq, Anthropic, OpenRouter and Gemini, not against live accounts. Kilo and Pollinations document few headers, so 429s are their main signal. |
| `ip_echo` default | `https://api.ipify.org` is a third-party call from every route. Set your own echo URL or `ip_echo: []` (direct IPs still come from the hub). |
| Shared-IP knowledge on nodes | The hub knows that two nodes share an exhausted IP (placement uses it). A node learns it only from its own first 429. Pushing hub buckets to nodes is a possible refinement. |
| Hub-local node as relay, hub as relay target | Supported by the code (unix-socket Tunnel calls are trusted like the link) but not in the suites. |
| CA rotation | Manual: replace the CA and re-enroll nodes (RUNBOOK §4.4). |
| Disk limits | Soft only. Hard limits (XFS/btrfs quotas) are Phase 5, as decided. |
| Live editing from the web UI, "promote to YAML" | Phase 4; the CLI and API exist now. |
| Template `outputs:` (harvest on exit) | Phase 3 with the artifact store. |

## Measured footprint

Release builds, idle and connected, measured with `/proc/<pid>/status` VmRSS (budgets from ARCHITECTURE §2: supervisor < 60 MB, node < 40 MB):

| Process | RSS |
|---|---|
| `ocl-supervisor` | 12.4 MB |
| `opencode-node` (worker) | 14.3 MB (hub's own node: 14.0 MB) |
| `opencode-node` (relay, `role: relay`) | 12.3 MB |

OpenCode itself: ~420 MB per session (unchanged from Phase 1). Its home directory takes ~120 MB on disk per instance, mostly provider packages; keep that in mind when choosing `container.disk`.
