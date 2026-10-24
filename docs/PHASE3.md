# Phase 3 report: artifacts, shared workspaces across machines, questions, dead agents, harvesting

Status: **done and verified end to end** on 2026-10-24, with OpenCode 1.19.4, the scripted mock LLM and the `process` runtime. Podman was not available on this test machine (see "Not verified here").

Design: [PHASE1_5.md](./PHASE1_5.md) §6–§9. Usage: [USAGE.md](./USAGE.md) §19–§22. Operations: [RUNBOOK.md](./RUNBOOK.md) §6. Config keys: [CONFIG.md](./CONFIG.md).

## What was built

| Piece | Where | Notes |
|---|---|---|
| Artifact store | `supervisor/src/artifacts.rs`, `shared-protocol/proto/link.proto` (`Blobs` service) | BLAKE3 content addressing, blobs on the hub disk, metadata + lineage in SQLite. Uploads declare their hash, and the hub skips bytes it already has. Downloads are verified. `artifacts.max_size`. |
| Trees and safe file access | `shared-protocol/src/archive.rs` | Manifests, deterministic tar.zst, size-capped extraction of plain files and directories only. `O_NOFOLLOW` + `/proc/self/fd` checks on every workspace open, so links never lead outside. Per-file three-way merge. |
| Node cache and workspace operations | `node-daemon/src/{cas,wsops}.rs` | LRU blob cache (`artifact_cache`), snapshot / merge / place, agent `artifact_put/get`, glob outputs as one tree, workspace `seed:`. |
| Shared workspaces across machines | `supervisor/src/workspaces.rs` | Sync before a session starts on a node whose copy may be behind, after each turn towards other machines with live sessions, and on `cluster_sync_workspace`. Conflicts stay local plus `<file>.theirs`, and the agents are told. Workflow steps may run on different machines. |
| Questions | `supervisor/src/questions.rs`, node interception in `manager.rs` | `cluster_ask`, OpenCode's `question` tool, `ask` permissions and budget stops. Routing up the chain, `cluster_answer/escalate`, timeouts, the four modes (live and per task), rules that only raise importance, operator-only questions, the assumption log, notifications. |
| Dead agents | `supervisor/src/recovery.rs`, node `tick`/`mark_dead`/`restart`/`halt` | Detection: exit code (128+signal from the bridge), OOM (Podman), wall clock, lost node, container gone. Priced `restart` / `resume_elsewhere` / `handoff` / `abandon`. Automatic within `on_death`; otherwise a halt of the whole tree with containers kept, resumed by one call. Checkpoints every N turns. |
| Harvest and clean-up | node `harvest`/`stop`/`reap`, hub `on_harvested` | `outputs:` (files, directories, globs), the transcript, delivery to the parent's / task's / any shared workspace on whichever machine holds it, and `retain:` (duration, `on_failure`, `always`, `never`). |
| CLI and API | `cli/`, `supervisor/src/api.rs` | CLI: `ocl questions [mode]`, `ocl answer`, `ocl assumptions`, `ocl artifact put/get/ls/show`, `ocl halts`, `ocl recover`, `ocl harvests`, `ocl notifications`, `ocl spawn --coordinator`, `ocl task add/edit --question-mode`. The matching REST endpoints exist under `/api/v1/*`, plus SSE events. |
| E2E suites | `tests/e2e/{artifacts,questions,death,harvest}.sh` | Four new suites, rerunnable on one machine. The six Phase 2 suites run unchanged. |

## Verified (each was run, not just compiled)

All ten suites passed back to back (`tests/e2e/run-all.sh`), **215 checks**:

| Suite | Checks | | Suite | Checks |
|---|---|---|---|---|
| enroll | 29 | | artifacts | 25 |
| relay | 14 | | questions | 32 |
| quota | 24 | | death | 38 |
| templates | 19 | | harvest | 17 |
| stats | 8 | | resilience | 9 |

Unit tests: 32. (The suite counter used to count each suite's "ALL PASSED" line as a check; Phase 2's 109 were really 103. Fixed.)

| Requirement | Evidence |
|---|---|
| Artifacts, operator side | A file and a tree (with an executable file and a link) go up and come back identical. The exec bit is kept; the link is not packed. The same content gives the same reference. |
| Artifacts between agents, lineage | An agent on the hub shares `greet.py` and a directory. An agent on worker-b fetches the file (it lands in its workspace) and publishes `derived.py`, whose parent is `greet.py`; `ocl artifact show` lists the child under `greet.py`. |
| One shared workspace on two machines | B (worker-b) started with A's files (synced at placement: `workspace demo: synced from hub`). B's new file reached the hub after B's turn, and A's next change reached worker-b. The same file was changed on both sides: the hub kept its version, wrote B's as `conflict.txt.theirs`, and the agent on the hub was told. |
| Workflow steps on different machines | `build` ran on the hub and `review` on worker-b. Its `exit_code: pytest` check ran on worker-b's copy of the run workspace and passed. Before the fix described below, the same check failed with "no tests ran", which is how the bug was found. |
| Questions reach the operator | `cluster_ask` from a top-level agent arrived as `normal` and `blocked 1`, with a notification. `ocl answer --choice 2` reached the agent as a message, and it wrote `9090`. |
| OpenCode's question tool | The session was `blocked`, and the inbox showed OpenCode's options. `ocl answer … postgres` replied through `/question/{id}/reply` and the session finished. |
| The chain answers | A child's `low` question went to its parent, which answered (`answered_by` = the parent); the child received it. |
| Escalation with a note | The parent escalated a `normal` question; it reached the operator with the note "parent does not know the deployment port", and the operator's answer reached the child. |
| Rules only raise importance | A `low` question about deleting data became `vital` (`mentions "delete"`). It went through the parent first and reached the operator in `vital_only`. |
| Operator-only | A question about a password went straight to the operator (`operator only`); the parent got an FYI. |
| Autonomous mode, assumption log | The parent escalated, nobody above would answer, so the parent decided (`decided` by the parent). The child acted on it and the decision is in `ocl assumptions`. A task can have its own mode. |
| Timeout | A parent that ignored the question: after `answer_timeout_secs` (6 s in the test) it moved to the operator (`escalated by timeout`). "Let the agent decide" reached the child. |
| Permissions as questions | `rm -rf build` became a `vital` permission question. `ocl answer … reject` rejected it; the command did not run, the agent was told, and it continued without it. |
| Spending past a budget | The keyproxy refusal raised a `vital`, operator-only budget question. `raise 1.00` raised the budget live, and the session finished. |
| Death detected, restart in place | OpenCode killed mid-step: `session.dead` with `exit:137`, then `recovery.auto` with `restart`. The agent finished in the same OpenCode session (history kept), with `deaths` = 1. |
| Halt when recovery is not cheap enough | With `max_auto_cost_usd: 0`, the child's death halted the tree from its parent: the parent was `halted` and the child `dead/exit:137`. The options were priced, and the reason was in the vital notification. `ocl recover <h> restart` finished the child and resumed the parent; the halt was then `resolved`. |
| Resume on another machine | `ocl recover <h> resume_elsewhere --node worker-b` started a replacement on worker-b (`replaces` = the dead one). Its workspace held the dead instance's files, and it was briefed with the original prompt and todos. |
| Lost node, checkpoint | worker-b and its sessions were killed. After the grace period: `node_lost`, then an automatic resume on the hub from the turn-1 checkpoint. The harvest record says anything written after the checkpoint is unrecoverable. |
| Wall clock | `dead/wall_clock` and halted, with the reason it was not retried. |
| Harvest on stop | The parent stopped its child. `report.md` was delivered into the parent's workspace at `dest`. Two wheels (not the sdist) became one tree artifact; `missing.txt` was reported as missing; the transcript artifact holds the conversation. `retain: never` removed the volumes at once. |
| Retention default, task end | Default `retain: 24h` kept the workspace and home after a stop. When a task completed, its worker was harvested with trigger `task_end` into the task workspace. |
| Phase 1 and 2 behaviour | The six Phase 2 suites pass unchanged. The Phase 1 demo (`run-local.sh --with-worker`, process runtime) still passes: the workflow passed, the task was done with the mid-run docstring in `greet.py`, and the audit chain verified (20 entries). |

Unit tests added in this phase:
- pack/unpack round trip (links skipped, size cap);
- no escape through links for reads and writes;
- the three-way merge cases;
- the importance rules (only raise) and the mode table;
- retention parsing and output validation.

## What the design changed while building

1. **Workspaces are synced by snapshot and a per-file three-way merge, not git bundles.** It works for any tree and needs no git in the workspace. Conflicts are explicit `.theirs` files that agents can reconcile; that is easier for a model than merge markers inside a file it is editing.
2. **`Blobs` is its own gRPC service on the link connection,** like `Tunnel`, so a large transfer never delays commands. Uploads carry their hash first, and the hub records metadata without reading the bytes when it already has the blob.
3. **`cluster_ask` returns at once,** and the answer arrives as a message. The asker shows `waiting on question(s)`. Agents that must stop and wait use OpenCode's own `question` tool, which blocks natively. A long-blocking MCP call would hit tool-call timeouts.
4. **After a rejection the agent is told.** OpenCode ends the turn when a permission is rejected or a question dismissed. In a cluster nobody may be watching, so the hub follows up with "refused by …, continue without it".
5. **`lost` stays a state name** and counts as a death (`node_lost`) for the recovery policy. The design's `dead` is used for containers that ended.
6. **A halt of a session without a task covers its whole tree,** from the top ancestor down, so a parent waiting on a dead child is paused too.
7. **The wall clock and crash loops are never retried automatically.** Giving an agent more time is the operator's call.
8. **A replaced session on a node that comes back is stopped,** so two instances never work on the same thing.
9. **The operator can start coordinators without a task** (`ocl spawn --coordinator`). This was needed for agent trees outside the task bucket.
10. **The bridge reports a signal death as 128+signal,** like shells and Podman (a kill or OOM kill shows as 137).
11. **Every harvested output is also an artifact,** with lineage to what the instance fetched, whatever its destination.
12. **Each new instance home gets an empty model catalog.** Without one, OpenCode fetches models.dev at boot. On a host whose DNS prefers unreachable IPv6 that stalled every new instance for ~30 s (38 s → 4 s here). Containers have no network and fail at once, so this mostly helps the `process` runtime.

## Not verified here / known limitations

| Item | Status |
|---|---|
| **Podman runtime** with Phase 3 code | Not run here. Untested Podman-only paths: death detection through `podman ps` + `inspect` (`OOMKilled`, exit code); `podman start` of a kept container for restart and halt resume; chown of files the node writes into workspaces under rootful Podman. **Run `tests/e2e/death.sh` logic against a Podman host, or at least `examples/quickstart/run-local.sh`, before relying on it.** |
| `resume_elsewhere` | Starts a fresh OpenCode conversation, briefed with the original prompt, todos and last output. OpenCode 1.19.4 cannot import a transcript through its API, so the old transcript is kept as an artifact rather than replayed. |
| Halt resume | Halted sessions are restarted with "continue where you left off". The step that was interrupted is not replayed exactly. |
| `handoff` | Relies on the parent agent to start the replacement (it gets a briefing message). |
| Lost node harvest | Records the latest checkpoint tree as the output. Per-pattern outputs are not extracted from it. |
| Several questions in one OpenCode request | Each question gets the same answer. |
| Sync cost | Each sync hashes the whole tree (the upload is skipped when the manifest is unchanged). Very large workspaces shared across machines will feel it. |
| Notifications | In-app list and SSE only. Web Push and ntfy are Phase 4, with the UI inbox. |
| Artifact retention and access | No garbage collection of hub blobs yet (Phase 5). Any approved node and any `read` token can fetch any artifact. |
| Cost estimates | Context size × the price in `providers.yaml` (`$0` without a price). They decide whether recovery is automatic, so set prices for paid models. |

## Measured footprint

Release builds, idle and connected (`VmRSS`). Budgets from ARCHITECTURE §2: supervisor < 60 MB, node < 40 MB.

| Process | Phase 3 | Phase 2 |
|---|---|---|
| `ocl-supervisor` | 14.1 MB | 12.4 MB |
| `opencode-node` (hub's own / worker) | 15.7 / 15.7 MB | 14.0 / 14.3 MB |
| `opencode-node` (relay) | 13.9 MB | 12.3 MB |

Artifacts stream end to end:
- a 64 MiB put and get through the hub raised the supervisor to 17.3 MB;
- `ocl artifact put` / `get` of 64 MiB peaked at 11 / 10 MB.

Binary sizes: supervisor 16.7 MB, node 21.6 MB, `ocl` 10.0 MB, `ocl-bridge` 1.2 MB (zstd and tar added about 3 MB per daemon).

A new instance now boots in about 4 s on this host, down from 38 s (design change 12).
