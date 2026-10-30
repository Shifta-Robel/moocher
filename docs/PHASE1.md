# Phase 1 report: config store, node daemon, local supervisor

Status: **done and verified end to end** on 2026-10-22, on Ubuntu 24.04 with Podman 4.9.3 (rootful), OpenCode 1.19.4 and the scripted mock LLM.

## What was built

| Piece | Where | Notes |
|---|---|---|
| Config store + schema | `shared-protocol/src/config.rs`, `node-daemon/src/cfgstore.rs` | Per-agent YAML under `agents/`, plus `node.yaml`, `providers.yaml`, `routes.yaml`, `supervisor.yaml` and `workflows/`. Hot reload with a debounced watch; a broken file keeps the last good config. JSON Schemas via `moocher-node schema`. |
| `moo-bridge` | `bridge/` | 1.2 MB static binary and the container entrypoint. UDS⇄TCP both ways, forwards signals. |
| Agent image | `images/agent/Containerfile` | OpenCode 1.19.4 from npm, plus bridge, git, python3 and pytest. 421 MB. |
| `moocher-node` | `node-daemon/` | Podman runtime (plus `process` runtime for dev), OpenCode client, event handling, keyproxy, cluster MCP, outbox, reconcile, wall-clock limits. |
| `moocher-hub` | `supervisor/` | SQLite state, gRPC node link (unix socket), REST + SSE API with scoped tokens, audit chain, workflows, task bucket + coordinator, `/metrics`. |
| `moo` CLI | `cli/` | Every API action, `--json` on all commands, `events -f` live. |
| Demo kit | `examples/` | Hub config, mock LLM with phase scripts, `run-local.sh`. |

## Verified (each was run, not just compiled)

| Requirement | Evidence |
|---|---|
| Turn-based handoff | `build-review` workflow: build PASS (todos 3/3 + `pytest` exit 0) → review PASS (`VERDICT: APPROVED`) → run **Passed**. A failing review routes back to build with feedback, and nudges fire on open todos. |
| Task bucket + main agent | Urgent task → coordinator → `cluster_spawn_agent(builder)` → notified when the builder's turn ends → spawns the reviewer → `cluster_task_complete` → task **done** with summary; all task sessions stopped. `when_free` waited for an idle cluster; `on_demand` stayed queued. |
| Mid-run operator command | `moo send <builder> "Please also add a docstring"` while it worked. The final `greet.py` contains the docstring. |
| Token usage + cost | Per session and in total, live in `moo status`, and per agent in `/metrics`. Cost comes from the `providers.yaml` price table when the provider reports none. |
| Secrets never reach agents | Mock provider rejects any key but the real one, and all calls succeeded. Inside the container: real key found 0 times; forged capability → 401; other model → 403. |
| Container isolation | `--network=none` (outbound connect refused), all capabilities dropped, read-only rootfs and read-only config, memory and pids limits applied. |
| Supervisor restart doesn't orphan | Killed the supervisor mid-run for 20 s. The node buffered 25 events, replayed them on reconnect (outbox → 0), and the session ended with correct tokens, todos and verdict. |
| Node restart doesn't orphan | Restarted the node mid-workflow. It re-attached to the running container, detected the finished turn, and the workflow continued. |
| Hub runs its own sessions | All demos ran on the hub's own node. A second local node (`worker-a`) registered and took placements via `--node`. |
| Audit log | Every control action is recorded with its actor (operator token, `session:<id>`, system). The chain verifies, and the unit test proves tamper detection. |

Run it yourself: `examples/quickstart/run-local.sh --with-worker` (see QUICKSTART.md).

## What the Phase 0 design got wrong (and what we did)

1. **The v2 session API lacks MCP tools in OpenCode 1.19.4.** The first coordinator run failed with "Unknown tool: cluster_list_agents". Sessions now run on the v1 API. Telemetry moved from the v2 durable stream to the v1 `/event` bus plus a full resync from message history after every reconnect. Usage is keyed per message, so a resync never double counts.
2. **OpenCode startup races.**
   - `OPENCODE_CONFIG` loads lazily, so the first prompt failed. We use `OPENCODE_CONFIG_DIR`.
   - That dir must contain `.gitignore`, because OpenCode writes one and our mount is read-only. We pre-create it.
   - The model catalog fills in after `/api/health` reports OK, so the node waits for its model in `/api/model`.
3. **One container per session, not per agent.** MCP calls can't be attributed to a session when several share a container. Measured cost: ~420 MB RSS per idle OpenCode.
4. **Mid-run delivery semantics.** In v1, a message sent while busy is picked up at the next step. That is our `queue`. `steer` aborts the current step first.
5. **The Podman CLI instead of the libpod REST crate.** Fewer moving parts, and identical across rootful and rootless setups.

## Known limitations (planned)

| Limitation | Phase |
|---|---|
| Nodes connect over a local unix socket only; remote nodes need mTLS + enrollment | 2 |
| `via_node` egress routes are rejected at config validation | 2 |
| Shared workspaces work on one node only; cross-node sync through artifacts | 3 |
| No web UI/TUI yet (API is ready: tasks reorder, SSE stream, tokens) | 4 |
| Operator password login, built-in TLS for the API | 4 |
| Keyproxy budgets use OpenCode's counters, not provider-response parsing | 5 |
| Load test at ~10 sessions, Quadlet/systemd units, event-log export | 5 |
| Container cold start ~35 s (OpenCode boot + catalog) | 5: pre-warmed pool |

## Upstream notes for OpenCode

- The v2 runner should expose MCP tools; then we can use durable replay and `delivery: steer|queue`.
- `serve --unix-socket`, which would remove half of `moo-bridge`.
- `OPENCODE_CONFIG` should be honoured at boot, like `OPENCODE_CONFIG_DIR`.
- Don't write `.gitignore` into a read-only config dir.
- `/api/health` should only report OK once the model catalog is ready.
- `/experimental/tool` returns a 500 in 1.19.4.
