# Phase 0 — Evaluation notes, config draft, risks, open questions

## 1. Session-control spike (done 2026-10-22)

Installed `opencode-ai@1.19.4`, ran `opencode serve`, and dumped `/doc` (OpenAPI 3.1, ~480 KB). Findings:

| Capability we need | OpenCode endpoint | Notes |
|---|---|---|
| Create session | `POST /api/session` `{agent, model{providerID,modelID}, location}` | Client may supply the `id` (`ses…`), so we generate IDs and keep them idempotent across retries. |
| Prompt | `POST /api/session/{id}/prompt` `{prompt{text,files,agents}, delivery: steer\|queue, resume}` | "Durably admit one session input". **Mid-run operator commands are native.** |
| Stop | `POST /api/session/{id}/interrupt` | Legacy `/session/{id}/abort` is also present. |
| Wait for idle | `POST /api/session/{id}/wait` | Used by evaluators and handoffs. |
| Live + replayable events | `GET /api/session/{id}/event?after=<seq>` | Durable aggregate `seq`, so a node can reconnect without losing events. |
| Token / cost | `session.next.step.ended.data.{tokens{input,output,reasoning,cache{read,write}}, cost}` | Per step. We sum per session. |
| Blocked detection | `/api/session/{id}/permission`, `/question` + events | Maps to `blocked`. |
| Active list | `GET /api/session/active` | Used by reconcile. |
| Server auth | `OPENCODE_SERVER_PASSWORD` (basic auth) | We set a random password per container anyway (defence in depth behind UDS). |
| Listen | `--hostname/--port` only. **No unix-socket flag.** | Hence `ocl-bridge`. |

**Conclusion:** "session control" means using the v2 HTTP API directly. The thinnest possible shim is `ocl-bridge`, and it handles transport only.

### Upstream feature requests to file (nice to have, not blocking)
1. `opencode serve --unix-socket <path>`. This would let us drop half of `ocl-bridge`.
2. A documented stability or deprecation policy for the `/api/*` (v2) surface, with semver on the OpenAPI `info.version` (currently `1.0.0` regardless of release).
3. Per-session token/cost budget enforcement inside OpenCode. We enforce it in keyproxy, but native support would fail more gracefully.
4. A provider-level `apiKey` env indirection that is guaranteed not to appear in `/config` responses. This is moot with keyproxy, but would help other users.

## 2. Config store — draft schema (`~/.opencode-cluster/`)

```
~/.opencode-cluster/
  node.yaml              # node identity, supervisor URL, limits, secret backend
  providers.yaml         # provider templates (shipped defaults + overrides)
  routes.yaml            # named egress routes
  agents/<name>.yaml     # one file per agent profile  ← unit of organization
  workflows/<name>.yaml  # (hub only) orchestration graphs
  nodes/<name>.yaml      # (hub only) pre-registered node manifests
  pki/                   # node key + cert (0600)
```

```yaml
# agents/builder.yaml
apiVersion: ocl/v1
kind: Agent
name: builder
description: "Writes Python modules from a spec; small, fast model."
provider:
  id: openrouter                 # → providers.yaml
  model: qwen/qwen3-coder
  key: { ref: "systemd-creds:openrouter" }   # or env:/file:/pass:; omit for keyless
  fallback: [{ id: kilo-free, model: auto }]  # keyless failover
route: via-vps-de                # → routes.yaml (direct | via_node | proxy)
role:
  system_prompt_file: prompts/builder.md
  opencode_agent: build          # built-in OpenCode agent mode to start in
tools:                           # rendered into OpenCode `permission` config
  edit: allow
  bash: { default: ask, allow: ["pytest *", "python *", "git *"] }
  webfetch: deny
  mcp: [cluster]                 # cluster MCP always injected; others optional
plugins: []                      # optional OpenCode plugins (e.g. opencode-plugin-otel)
container:
  isolation: agent               # agent | session
  image: ghcr.io/you/ocl-agent:1.19.4
  memory: 1g
  cpus: 1.5
  pids: 512
  egress: none                   # none | proxy(allowlist) | full
limits:
  max_sessions: 2
  budget_usd: 2.00               # per session, enforced by keyproxy
  max_tokens: 400000
  wall_clock: 45m
workspace:
  mode: isolated                 # isolated | shared
  name: demo-project             # shared: same name ⇒ same workspace
success:
  todos_required: true           # PASS needs all todos completed
  max_nudges: 2
  checks: []                     # optional: [{exit_code: "pytest -q"}]
labels: { team: demo }
```

Hot reload uses `notify` on the config dir.
- `description`, `limits` and `route` apply live, through keyproxy and the scheduler.
- `role`, `tools` and `container` apply to *new* sessions. Running sessions get a `config_drift` flag in the UI.
- Every file is validated against a JSON Schema exported from `shared-protocol` before it is applied. An invalid file keeps the last good version and raises an event.

## 3. Pre-registration flow
1. On the hub, the operator writes `nodes/gpu-box.yaml`. It lists the expected agents (name, description, desired count) and placement labels, but **no secrets**.
2. `ocl node invite gpu-box` prints a one-time enrollment token.
3. On the worker, run `ocl node enroll --supervisor https://hub:7443 --token …`. This produces the key and CSR, installs the cert, and enables the systemd unit.
4. On connect, the node sends `Hello{node, version, capacity, agents:[{name, config_hash, secret_resolvable: bool}]}`.
5. The supervisor matches the node against its manifest. Matching agents become **eligible** for orchestration. Mismatches are shown as warnings, for example: "expected `reviewer`, missing", or "secret unresolvable".
6. On disconnect, sessions move to `unreachable`, not `lost`, with their metadata and history kept. After the grace period (default 10 min) they become `lost` and the workflow's `on_lost` policy applies.

## 4. Resource plan for 8 GB / 4 vCPU nodes

| Item | RSS |
|---|---|
| opencode-node | ~30 MB |
| OpenCode container (idle/active) | ~250 / ~450 MB (estimate; to be measured in Phase 1) |
| Practical ceiling | ~8–10 concurrent sessions with `isolation: agent` and 2 sessions/container |

The Phase 5 load test targets 10 concurrent sessions spread over hub + 2 workers.

## 5. Risks

| Risk | Mitigation |
|---|---|
| The OpenCode v2 API changes between releases (the `experimental/*` routes clearly will) | Pin the version in the image. Generate the client from `/doc`. A CI job diffs the schema on new releases. Use only non-experimental routes. |
| Keyless gateways are unreliable or rate-limited | Mark them `tier: best_effort`. Keyproxy fails over to `fallback` providers. Report a separate error class in the UI. |
| Shared workspace edited by two agents at once | Advisory locks via MCP, and coordination through todos and messages. Cross-node sync uses git, so conflicts are explicit. |
| Agent marks todos complete without doing the work | Optional hard checks per step. The demo runs `pytest`. |
| Agent exfiltrates its capability token | The token only works through the node UDS. It is budget-capped and revoked on session end. The container has no network, so the token can't be used anywhere else. |
| Rootless Podman differences across distros (cgroup v2 delegation for `--memory`) | `ocl doctor` preflight checks cgroup v2, subuid/subgid and the podman version. The runbook covers the fixes. |
| SQLite contention under load | WAL mode, one writer task, batched event inserts. |

## 6. Decisions from operator review (2026-10-22)

| Question | Decision |
|---|---|
| Web UI framework | **SolidJS** (+ Vite), static build embedded in the supervisor. Mobile-first. |
| Workspaces | **Configurable** `isolated` / `shared`. Same-node sharing mounts one Podman volume into several containers. Cross-node sharing syncs git bundles through the artifact store. No `podman cp`/SCP anywhere (ARCHITECTURE §6b). |
| Success signal / LLM judge | **Todo-list based.** Use OpenCode's native todos, roll them up into a shared task board, with optional hard checks. The LLM judge is dropped from v1 (ARCHITECTURE §6a). |
| Hub hosting agents | ~~Off by default~~ → **On** (operator request in review 2): the hub runs its own `opencode-node` over a local socket (D9). |
| Egress routing | `direct`, **`via_node`** (send LLM calls out through another cluster machine's IP; TLS stays end to end and the key stays put) and `proxy` (any SOCKS/HTTP). WireGuard netns is dropped (D10). |

Later additions (review 2): a **task bucket** with `urgent` / `when_free` / `on_demand` labels and a coordinator "main agent" (ARCHITECTURE §5a), and **adding machines from the web UI** with one-time join tokens (ARCHITECTURE D5).

## 7. Remaining open items (none blocking Phase 1)
- `via_node` path: always relay through the hub (simplest), or allow direct node-to-node mTLS when reachable? **Plan:** hub relay in v1, direct links later.
