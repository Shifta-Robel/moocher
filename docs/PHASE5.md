# Phase 5 report: hardening, metering, operations, load and chaos

Status: **done and verified end to end** on 2026-10-24, with OpenCode 1.19.4, the scripted mock LLM and the `process` runtime, on a 4-core machine. Podman was not available here, and neither were XFS/btrfs quotas or a running systemd (see "Not verified here").

Usage: [USAGE.md](./USAGE.md) §15, §28–31. Operations: [RUNBOOK.md](./RUNBOOK.md) §8. Config keys: [CONFIG.md](./CONFIG.md).

## What was built

| Piece | Where | Notes |
|---|---|---|
| Usage metering | `node-daemon/src/usage.rs`, `gateway.rs` | keyproxy reads the usage the provider reports, from JSON bodies and SSE streams, as they pass. It understands OpenAI chat, the Responses API, Anthropic and Gemini, and adds `stream_options.include_usage` to OpenAI-compatible streams. Budgets and token caps use the larger of OpenCode's count and the meter's. `usage.mismatch` is raised when the meter is clearly ahead. The metered count is kept with the session report, so it survives node restarts. It is shown in the CLI and the dashboard and used by stats and recovery estimates. |
| Egress allowlist | `node-daemon/src/egress_proxy.rs`, `runtime.rs`, `config.rs` | `container.egress: allowlist` with `allow_hosts`. The container keeps `--network=none`; the bridge forwards `127.0.0.1:3128` to a per-instance proxy on the node, which accepts CONNECT and plain HTTP. The list is read live. A listed name that resolves to an internal address is refused. Refusals are recorded as `egress.denied`. |
| Hard disk limits | `node-daemon/src/disk_quota.rs` | `disk_enforce: hard` puts the instance's home and its isolated workspace under one XFS project quota or btrfs qgroup. The node checks the filesystem, its mount options and root at start, and refuses to run where quotas cannot work. Quotas are cleared at reap. |
| Housekeeping | `supervisor/src/retention.rs` | Hourly. Events are exported by UTC day to `exports/events-YYYYMMDD.jsonl.zst`; the export is incremental by event id, appending zstd frames. Only exported events are pruned. Artifacts expire after `artifacts.retain`, except those tagged `keep` and anything still referenced. `ocl housekeeping [--dry-run] [--ahead 1d]` runs it on demand; `ocl-supervisor events-cat` reads exports. Audited. |
| systemd and Quadlet | `deploy/`, `images/supervisor/Containerfile` | A hardened supervisor unit, a rootful node unit, a rootless node user unit, and a Quadlet plus image for the supervisor. `tests/deploy-check.sh` runs `systemd-analyze verify` and `security`, and Quadlet's generator. |
| Load and chaos | `tests/e2e/load.sh` | 12 instances on 3 nodes under a shared allowance that is too small. Then a worker is killed with 4 instances mid-task, then the supervisor is killed under load. Latency and memory are reported. |
| Security audit | `tests/audit.py` | An offline `cargo audit` against the RustSec database. |
| E2E | `tests/e2e/{metering,egress,ops,load}.sh` | Four new suites. The mock LLM now sends stream usage only when asked (like OpenAI) and has a `/v1/_stats` endpoint. |

## Verified (each was run, not just compiled)

All seventeen suites passed in one run (`tests/e2e/run-all.sh` with `PEBBLE_DIR` and `PW_DIR`): **339 checks**. Unit tests: 50.

| Suite | Checks | | Suite | Checks |
|---|---|---|---|---|
| enroll | 29 | | harvest | 17 |
| relay | 14 | | web | 38 |
| quota | 24 | | acme | 10 |
| templates | 19 | | ui | 3 (17 browser steps) |
| stats | 8 | | **metering** | 15 |
| resilience | 9 | | **egress** | 13 |
| artifacts | 25 | | **ops** | 19 |
| questions | 32 | | **load** | 26 |
| death | 38 | |  |  |

| Requirement | Evidence |
|---|---|
| keyproxy meters usage itself | On an ordinary turn the meter and OpenCode agree to the token, and every streamed request reached the provider asking for usage. Two direct calls with the capability token (what an agent with a shell could do) moved the meter by 100k tokens while OpenCode's count stayed put. `usage.mismatch` was recorded and notified. The next call was refused with the budget message, and the budget question went to the operator. The metered count survived a node restart. Parser unit tests cover all four formats, streams split at arbitrary points, and a stream with no usage. |
| No false alarms under load | 12 concurrent instances raised no `usage.mismatch`. |
| Egress allowlist | From the agent's own shell, through `HTTP_PROXY`: a listed host gave 200 over plain HTTP and over CONNECT. An unlisted port got 403 on both, and that server saw no request. `localhost` (listed, but it resolves to loopback) got 403 with the reason. Removing a host refused it on the next connection. A profile with an allowlist but no hosts was rejected. |
| Hard disk limits | `disk_enforce: hard` on this machine (overlay, not root) refused to start: "needs XFS (prjquota) or btrfs under the data dir; / is overlay". Unit tests cover the mountinfo parsing (including escaped paths and missing `prjquota`) and the exact `xfs_quota` / `btrfs` command plans. |
| Event export and pruning | Nothing was exported before the day ended. A run as of tomorrow wrote `events-<today>.jsonl.zst`, which decodes to one event per line; a second run exported nothing; later events were appended as a new frame. A dry run changed nothing. Pruning removed only exported events, and the export still held them. |
| Artifact expiry | A dry run listed only the plain artifact. The real run removed it and its blob. The one tagged `keep` stayed, and so did one an agent profile seeds from. Audited, with an `artifact.expired` event. |
| systemd units | `systemd-analyze verify` (systemd 252) passed for all three units. `security`: supervisor 1.6 "OK", node 7.6 (Podman needs mounts and namespaces; the agents' own sandbox is their container). Quadlet generated a unit running a read-only container with no capabilities and `no-new-privileges`. |
| Load | 12 instances, 4 per node, all finished in 85–89 s. More requests than the shared per-IP allowance (40 per 30 s, every node out of 127.0.0.1): 2–9 sessions parked (depending on timing) and resumed after the reset. 1–4 of ~62 provider calls got 429 (see "known limitations"). No session saw an error, and every instance wrote its result. |
| A worker dies mid-task | All 4 of its sessions were declared `node_lost` after the grace period. Each was resumed on another node and finished, or halted for the operator when resuming would cost more than the policy allows (both outcomes occurred across runs). Nothing was placed on the dead node. |
| The supervisor dies under load | `kill -9` with 6 sessions running, restarted 5 s later. All 6 finished, each turn end was recorded once, no session was falsely declared lost, the audit chain verified, and tokens and the meter were intact. |
| Memory under load | Peak VmRSS with 12 instances (debug builds): supervisor 32 MB, nodes 32–33 MB. Budgets: < 60 MB / < 40 MB. |
| Security audit | See below. |
| Phases 1–4 | The thirteen earlier suites passed in the same run. |

## Bugs found by the new tests, all fixed

1. **False `node_lost` after a hub restart.** A restarted supervisor counted a node's grace from its last contact *before the crash*. It declared every session lost 8 s after starting, while the nodes were still backing off; the sessions came back but their halts stayed open. Now nothing is declared lost in the first 90 s after start (nodes back off up to 60 s), a node's grace counts from the later of its last contact and the hub's start, and a lost session that reports in alive has its halt resolved and its paused tree resumed.
2. **Premature turn end under load.** OpenCode's `prompt_async` returns before the session is marked busy. Under load, the check 300 ms later could still see "idle" and report a turn ended with no output. A turn now ends only once an assistant message created after the prompt exists. A one-minute escape hatch means a missing answer cannot leave a session running forever.
3. **False `usage.mismatch` under load.** OpenCode's token report for a step can arrive seconds later, sometimes only when its history is re-read at turn end. The comparison is now made at turn end, or 10 s after a call made between turns.
4. **Budget refusals lost on large requests.** keyproxy refused before reading the body, and a client still sending got a connection reset instead of the error. Requests are now read first.
5. The mock LLM always sent stream usage. It now does so only when asked, which is what exposed the need for `include_usage`.

## Security audit

`tests/audit.py` against the RustSec advisory database (2026-10-24), 477 crate versions:

| Advisory | Crate | Action |
|---|---|---|
| RUSTSEC-2026-0387: timing variability in "constant-time" operations | crypto-bigint 0.5.5, via p256 (Web Push ECDH, VAPID signing) | **Fixed**: p256 0.14 → crypto-bigint 0.7.7. The RFC 8291 test vector still matches byte for byte; VAPID JWTs verify; existing key files load. |
| RUSTSEC-2026-0002, -0253: unsound `IterMut`, use-after-free in `pop` | lru 0.12.5, via ratatui (`ocl tui`) | **Fixed**: ratatui 0.30 (lru 0.18.5), crossterm 0.29. |
| RUSTSEC-2024-0436: unmaintained | paste, via ratatui | **Gone** with ratatui 0.30. |
| RUSTSEC-2024-0449: unmaintained | unsafe-libyaml, via serde_yaml | **Accepted**: no known vulnerability; it parses operator-owned config only (agents cannot write config). Replacing serde_yaml is a separate change. |

`cargo-audit` itself was not installed (it needs a C toolchain for a static build here). The script applies the same database and version rules, and was checked against known-vulnerable versions (it finds RUSTSEC-2021-0003, -0078, -0079 and 2022-0022).

## What the design changed while building

1. **The allowlist proxy runs per instance on the node**, reached through the bridge, instead of giving containers a network. "No network" stays true for every container; the allowlist is just another forwarded socket.
2. **Hard limits refuse to start rather than fall back.** An operator who asked for `hard` and gets `soft` would believe writes are capped when they are not.
3. **Event export is incremental by id**, not "the previous day". A run triggered by hand, or events that arrive late, can then never be skipped or written twice.
4. **Artifact expiry is reference-aware and opt-in** (`retain` unset = keep everything), with `--dry-run` and `--ahead`.
5. **The warm pool was deferred** (see below).
6. **The supervisor image is glibc-based** (debian-slim), not static musl. musl needs a cross C toolchain for ring/zstd, and glibc is all the binary links.

## Not verified here / known limitations

| Item | Status |
|---|---|
| **Podman runtime** | Not run on this machine (as in Phases 2–4); rootless Podman fails here mounting `/proc`. The allowlist's Podman path (`--network=none`, the extra `--fwd`, proxy env) and the systemd/Quadlet units are untested at runtime. |
| **Hard disk quotas** | Only the command plans (unit tests) and the refusal (e2e). No XFS with `prjquota`, no btrfs, no root here. |
| **systemd** | Units verified statically (`systemd-analyze verify`/`security`, Quadlet dry run), not run under a real systemd. |
| **Warm pool** | Deferred. One instance boots in ~4 s, but OpenCode needs ~9 CPU-seconds to boot. On this 4-core machine, 12 at once took 28 s alone and 54–65 s (p50) under the load test, with debug daemons on the same cores. A pool would help bursty spawns on small hosts. It needs instance ids decoupled from sockets, config and capability tokens; that is the next step if start time matters. |
| **429 overshoot on a shared IP** | Each node learns a shared allowance's remaining count from its own responses, so near a window's end requests in flight on other nodes can overshoot. The overshoot is bounded by the number of concurrent requests (1–4 of ~62 here). Every 429 is absorbed (the session parks until the reset); none fails. |
| `ocl quotas` "USED" | For provider-learned limits it shows requests counted since tracking began, not within the current window (LEFT and RESETS are the window's). |
| Halts after node death | Which of the 4 lost sessions resume automatically depends on the cost estimate against `max_auto_fraction` (across runs, 2 to 4 of 4 resumed; in the final run all 4). This is the default policy as designed. |

## Measured footprint

Release builds: supervisor 22.4 MB (Phase 4: 21.5), node 22.3 MB (22.0), `ocl` 11.2 MB, `ocl-bridge` 1.2 MB. Under the 12-instance load (debug builds): supervisor 32 MB, nodes 32–33 MB.

| Latency (12 instances at once, 4 cores) | p50 | p95 |
|---|---|---|
| spawn → OpenCode ready | 54–65 s | 65–68 s |
| spawn → task finished (incl. parking for the allowance) | 79–84 s | 85 s |

(Ranges over the last two runs.)

A single instance on an idle host is ready in ~4–5 s.
