# opencode-cluster — Usage guide

Practical, copy-pasteable examples for every feature that exists today (Phase 1: one machine, hub + local nodes). Features from later phases are marked **(Phase N)** at the end.

Contents:
1. [Mental model in 60 seconds](#1-mental-model-in-60-seconds)
2. [One-time setup](#2-one-time-setup)
3. [Defining agents](#3-defining-agents)
4. [Everyday session control](#4-everyday-session-control)
5. [Messaging: queue, steer, inbox](#5-messaging-queue-steer-inbox)
6. [Permissions and blocked sessions](#6-permissions-and-blocked-sessions)
7. [Workflows (turn-based handoffs)](#7-workflows-turn-based-handoffs)
8. [The task bucket and the main agent](#8-the-task-bucket-and-the-main-agent)
9. [Workspaces and handing files between agents](#9-workspaces-and-handing-files-between-agents)
10. [Budgets, limits and egress routes](#10-budgets-limits-and-egress-routes)
11. [Monitoring: status, events, metrics](#11-monitoring-status-events-metrics)
12. [REST API cookbook (curl)](#12-rest-api-cookbook-curl)
13. [Access tokens and the audit log](#13-access-tokens-and-the-audit-log)
14. [Scripting recipes](#14-scripting-recipes)
15. [Running as systemd services](#15-running-as-systemd-services)
16. [Testing offline with the mock LLM](#16-testing-offline-with-the-mock-llm)
17. [Recovery and maintenance](#17-recovery-and-maintenance)
18. [What's coming](#18-whats-coming)

---

## 1. Mental model in 60 seconds

```
you ──ocl / curl──▶ ocl-supervisor (hub) ◀──gRPC── opencode-node (hub)  ──▶ podman: [ocl-bridge + opencode] × N
                                          ◀──gRPC── opencode-node (more)  ──▶ …
```

| Term | Meaning |
|---|---|
| **Agent (profile)** | A YAML file in `agents/`: model, key reference, role prompt, tool permissions, limits. It is a template. |
| **Session** | One running OpenCode conversation spawned from a profile, in its own container. It has an id like `s-3f9a1c2b7d`. |
| **Node** | A machine running `opencode-node`. The hub is a node too. |
| **Workflow** | A fixed sequence of steps (build → review), each a session, with pass/fail rules. |
| **Task** | An item in your bucket. A *coordinator* session (the main agent) does it using other agents. |
| **Todos** | Every agent keeps an OpenCode todo list. Its progress is how success is judged. |
| **Capability token** | What an agent holds instead of an API key. It only works through its own node, for its own model, within its budget. |

Session states: `starting → idle ⇄ running`, plus `blocked` (waiting on a permission), `error`, `stopped`, `unreachable` (node disconnected) and `lost`.

---

## 2. One-time setup

```bash
git clone <this repo> opencode-cluster && cd opencode-cluster
rustup target add x86_64-unknown-linux-musl
cargo build --release
cargo build --release -p ocl-bridge --target x86_64-unknown-linux-musl
podman build -t localhost/ocl-agent:1.19.4 -f images/agent/Containerfile .

# Put binaries on PATH (or use target/release/… directly)
sudo install -m755 target/release/{ocl,ocl-supervisor,opencode-node} /usr/local/bin/
sudo install -m755 target/x86_64-unknown-linux-musl/release/ocl-bridge /usr/local/bin/

# Start from the example config
mkdir -p ~/.opencode-cluster && cp -r examples/quickstart/hub/* ~/.opencode-cluster/
```

The hub's config directory now holds both halves:

```
~/.opencode-cluster/
  supervisor.yaml   node.yaml   providers.yaml
  agents/builder.yaml  agents/reviewer.yaml  agents/coordinator.yaml
  prompts/…            workflows/build-review.yaml
```

Initialise and start:

```bash
ocl-supervisor init          # prints an admin token once; also saved to ~/.opencode-cluster/cli-token (0600)
ocl-supervisor run &         # API on 127.0.0.1:7700, node socket /run/ocl/supervisor.sock
opencode-node check          # validates agents, resolves secrets (never prints them)
opencode-node run &          # the hub's own node
ocl agents                   # READY=yes for every agent you expect
```

`ocl` reads the token from `OCL_TOKEN`, or else `~/.opencode-cluster/cli-token`, and the server from `OCL_URL` (default `http://127.0.0.1:7700`). Add `--json` to any command for machine-readable output.

> If `/run` isn't writable for your user, set `runtime.run_dir` in `node.yaml` and `link_socket` in `supervisor.yaml` to e.g. `$XDG_RUNTIME_DIR/ocl`, and point `node.yaml: supervisor:` at the same socket.

### A second node on the same machine

Useful for testing placement, or for keeping two sets of keys apart:

```bash
mkdir -p ~/ocl-worker-a/agents
cp ~/.opencode-cluster/providers.yaml ~/ocl-worker-a/ && cp -r ~/.opencode-cluster/prompts ~/ocl-worker-a/
cp ~/.opencode-cluster/agents/builder.yaml ~/ocl-worker-a/agents/
cat > ~/ocl-worker-a/node.yaml <<'EOF'
name: worker-a
supervisor: unix:///run/ocl/supervisor.sock
max_sessions: 4
runtime: { run_dir: /run/ocl-worker-a }
EOF
opencode-node --config-dir ~/ocl-worker-a run &
ocl status        # two nodes, connected
```

---

## 3. Defining agents

One file per agent in `agents/`. Changes are picked up automatically: limits, keys and routes apply immediately, and role, tools and container settings apply to the next session. Run `opencode-node check` after editing.

### 3.1 BYOK coder on OpenRouter

```yaml
# agents/coder.yaml
name: coder
description: Implements features in Python/TypeScript with tests.
provider:
  id: openrouter
  model: qwen/qwen3-coder
  key: { ref: "env:OPENROUTER_API_KEY" }
role:
  system_prompt_file: prompts/coder.md
tools:
  edit: allow
  bash: { default: ask, allow: ["pytest*", "python3 *", "npm test*", "git status", "git diff*", "ls*", "cat *"] }
container: { memory: 1500m, cpus: 2 }
limits: { max_sessions: 2, budget_usd: 2.50, max_tokens: 2000000, wall_clock_secs: 3600 }
```

`OPENROUTER_API_KEY` must be in **`opencode-node`'s** environment, not in your shell for `ocl`. With systemd, use credentials instead (§15):

```yaml
  key: { ref: "systemd-creds:openrouter" }
```

### 3.2 Fast reviewer on Groq

```yaml
# agents/reviewer-fast.yaml
name: reviewer-fast
description: Quick code review. Ends with VERDICT APPROVED or CHANGES_REQUESTED.
provider: { id: groq, model: llama-4-maverick, key: { ref: "file:/etc/ocl/keys/groq" } }
role: { system_prompt_file: prompts/reviewer.md }
tools:
  edit: deny                         # reviewers read, they don't write
  bash: { default: deny, allow: ["python3 -m pytest*", "cat *", "ls*", "git diff*"] }
limits: { budget_usd: 0.30 }
```

### 3.3 Planner on Gemini, read-only

```yaml
# agents/planner.yaml
name: planner
description: Breaks a request into a concrete, ordered implementation plan.
provider: { id: gemini, model: gemini-3-pro, key: { ref: "pass:ai/gemini" } }
role:
  opencode_agent: plan               # OpenCode's planning mode
  system_prompt: |
    Produce a numbered plan with files to touch and tests to write.
    Do not modify files.
tools: { edit: deny, bash: deny }
```

### 3.4 Keyless agents (no API key at all)

```yaml
# agents/free-helper.yaml — Kilo gateway free models (the endpoint OpenKilo uses)
name: free-helper
description: Low-stakes chores on a free model.
provider: { id: kilo-free, model: <a free model id from Kilo> }
limits: { max_sessions: 1 }
```

```yaml
# agents/pollen.yaml — Pollinations (anonymous tier; add key: for a token)
name: pollen
description: Drafts docs and READMEs.
provider: { id: pollinations, model: openai }
```

Keyless gateways are marked `best_effort` in `ocl agents`. Expect rate limits.

### 3.5 Your own provider (any OpenAI/Anthropic/Gemini-compatible endpoint)

```yaml
# providers.yaml
providers:
  lab-vllm:
    base_url: http://10.0.0.20:8000/v1
    api: openai
    auth: { type: none }
    prices: { my-model: { input_per_mtok: 0, output_per_mtok: 0 } }
  company-proxy:
    base_url: https://llm.internal.example/v1
    api: anthropic
    auth: { type: header, name: x-api-key }
    headers: { X-Team: platform }
```

```yaml
# agents/local.yaml
name: local
description: On-prem model.
provider: { id: lab-vllm, model: my-model, context: 32768, max_output: 4096 }
```

### 3.6 A coordinator (the "main agent" for tasks)

The task scheduler starts the profile named in `supervisor.yaml: tasks.coordinator_agent` (default `coordinator`). Give it a strong model and a prompt about *how you like work delegated*:

```yaml
# agents/coordinator.yaml
name: coordinator
description: Main agent. Plans tasks and delegates to the team.
provider: { id: openrouter, model: anthropic/claude-opus-5-5, key: { ref: "env:OPENROUTER_API_KEY" } }
role:
  system_prompt: |
    You lead a team. Prefer: planner for anything non-trivial, coder for
    implementation, reviewer-fast for review. Run at most 3 agents at once.
    Share your workspace with agents that must see the code.
    When done, call cluster_task_complete with what changed and where.
tools: { edit: allow, bash: { default: deny, allow: ["ls*", "cat *", "git log*", "git diff*"] } }
limits: { budget_usd: 5.00, wall_clock_secs: 7200 }
```

The coordinator automatically gets extra tools (`cluster_spawn_agent`, `cluster_task_complete`, …) and a briefing. Other agents never get them.

### 3.7 Check what you defined

```bash
opencode-node check
#   ✓ agent coder            openrouter/qwen/qwen3-coder
#   ✗ agent reviewer-fast    groq/llama-4-maverick  [secret does not resolve]
ocl agents
opencode-node schema --out ~/.opencode-cluster/schemas   # JSON Schemas for your editor
```

---

## 4. Everyday session control

```bash
# Start a session (placed on any node that has the agent and capacity)
ocl spawn coder -p "Add a --json flag to cli.py and tests for it" --title "json flag"
# s-81c2d0a3f4 on hub

ocl spawn coder -p "…" --node worker-a          # pin to a node
ocl spawn coder -p "…" --shared payments-api    # use a named shared workspace (§9)

ocl sessions                 # live + recent (add --all for history)
ocl show s-81c2d0a3f4        # state, model, workspace, tokens, cost, todos, last output
ocl interrupt s-81c2d0a3f4   # abort the current step (session stays alive)
ocl stop s-81c2d0a3f4        # stop and remove the container (workspace kept)
ocl stop s-81c2d0a3f4 --rm-workspace   # isolated workspaces only
```

Example `ocl show` output:

```
s-81c2d0a3f4 · agent coder · node hub · running
model openrouter/qwen/qwen3-coder  workspace isolated:s-81c2d0a3f4
tokens in 41210 out 2210 reasoning 0 cache r/w 12000/0  cost $0.0183
todos:
  [x] add --json flag
  [>] write tests
  [ ] run tests
```

A session stays alive after its turn ends (`idle`), so you can keep talking to it (§5). Stop it when you're done; idle containers still use RAM.

---

## 5. Messaging: queue, steer, inbox

```bash
ocl send s-81c2 "Also handle the empty-input case."             # queue (default)
ocl send s-81c2 "Stop — use argparse, not click." --steer       # steer
ocl send s-81c2 "FYI: CI is red on main, not your fault." --inbox
```

| Mode | What happens | Use it for |
|---|---|---|
| `queue` | Picked up at the agent's **next step** (or starts a new turn if idle). | Most mid-run instructions. |
| `steer` | The current step is aborted, then the message is delivered. | Course corrections that can't wait. |
| `inbox` | Stored; the agent reads it when it calls `cluster_read_inbox`. | Background info that shouldn't interrupt. |

Messages from you arrive as `[message from operator <token-name>]`. Messages between agents arrive as `[message from session s-… (agent)]`.

### Agents messaging each other

Every agent has these MCP tools (OpenCode shows them as `cluster_*`):

| Tool | Example call by an agent |
|---|---|
| `cluster_list_sessions` | "who else is working right now?" |
| `cluster_session_status {session}` | read another agent's todos and last output |
| `cluster_send_message {to, text, delivery}` | "reviewer s-9a…: files are ready" |
| `cluster_read_inbox` | pick up stored messages |
| `cluster_list_agents` | see which profiles exist, and where |

Prompt pattern that works well:

```bash
ocl spawn coder --title "api" -p "Implement the /orders endpoint. When done, send a
message to session s-REVIEWER with cluster_send_message summarising what changed."
```

---

## 6. Permissions and blocked sessions

Anything set to `ask` in an agent's `tools` pauses the session until you answer:

```bash
ocl sessions
# s-81c2…  coder  hub  blocked  1/3  …  permission: bash ["rm -rf build"] (request per_7f…)

ocl permit s-81c2d0a3f4 per_7f2c… once     # allow this one call
ocl permit s-81c2d0a3f4 per_7f2c… always   # allow this pattern for the rest of the session
ocl permit s-81c2d0a3f4 per_7f2c… reject   # the agent is told it was refused
```

Tip: use `ask` as the default for `bash`, with an explicit `allow` list of safe commands. Most sessions then never block.

---

## 7. Workflows (turn-based handoffs)

A workflow is a YAML file in the hub's `workflows/`. Each step spawns a session. When it goes idle the supervisor judges it, then hands off to the next step. All steps of a run share one workspace.

### 7.1 Run the bundled one

```bash
ocl workflow ls
# build-review   Builder implements, reviewer approves or sends it back.  [build → review]

ocl workflow run build-review "Write slugify(text) in slug.py with pytest tests"
# run r-1a6a0a0485 started

ocl workflow runs
# r-1a6a0a0485  build-review  Running  40s  build:Pass → review:…
ocl workflow show r-1a6a0a0485 --json | jq '.steps[] | {step, verdict, check_results}'
ocl workflow cancel r-1a6a0a0485
```

### 7.2 A three-step plan → build → review with a fix-up loop

```yaml
# workflows/feature.yaml
apiVersion: ocl/v1
kind: Workflow
name: feature
description: Plan, implement, review; review failures go back to implementation.
max_step_runs: 7
steps:
  - name: plan
    agent: planner
    prompt: |
      Write PLAN.md for: {{input}}
      List files to change and tests to add. Do not write code.
    success:
      todos: ["write PLAN.md"]
      checks: [ { exit_code: "test -s PLAN.md" } ]

  - name: build
    agent: coder
    prompt: |
      Implement PLAN.md in this directory. Original request: {{input}}
      {{feedback}}
    success:
      todos: ["implement", "write tests", "run tests"]
      max_nudges: 2
      checks:
        - exit_code: "python3 -m pytest -q"

  - name: review
    agent: reviewer-fast
    prompt: |
      Review the change for: {{input}}
      Implementer's summary:
      {{prev.output}}
      End with exactly one line: VERDICT: APPROVED or VERDICT: CHANGES_REQUESTED.
    success:
      checks: [ { text_contains: "VERDICT: APPROVED" } ]
    on_fail: build          # reviewer's text arrives in build's {{feedback}}
```

```bash
ocl workflow run feature "Rate-limit the /login endpoint to 5 attempts per minute per IP"
```

How judging works for each step:
1. **Todos:** every seeded todo must exist and be `completed`, and nothing may be left `pending` or `in_progress`. If items are open, the agent gets a "continue with your open todos" nudge (up to `max_nudges`).
2. **Checks** run once the todos pass: `exit_code` runs inside that session's container, and `text_contains` inspects the agent's final message.
3. **PASS** goes to `next` (or the following step). **FAIL** goes to `on_fail`, with `{{feedback}}` set to the failing step's output and check results, or ends the run as failed.

Template variables: `{{input}}`, `{{prev.output}}`, `{{prev.step}}`, `{{prev.todos}}`, `{{run.id}}`, `{{feedback}}`.

> Phase 1 places a whole run on one node that has every step's agent (a shared workspace lives on one machine). Cross-machine runs arrive in Phase 3.

---

## 8. The task bucket and the main agent

Write tasks into the bucket; the cluster works through them on its own.

### 8.1 Adding tasks

```bash
ocl task add "Upgrade pydantic to v3 and fix breakages" --label when_free
ocl task add "Prod: 500s on /checkout since 14:00" -b "Stack trace in logs/checkout.txt. Find and fix." --label urgent
ocl task add "Write release notes for 2.4" --label on_demand
ocl task add "Add pagination to /orders" -t workflow:feature --label when_free   # run a workflow instead
ocl task add "Refresh docs" -t docs -t low-risk                                  # free-form tags
```

| Label | Starts… |
|---|---|
| `urgent` | immediately, when a task slot is free (one extra slot is reserved for urgent work) |
| `when_free` | only when no task is running **and** all sessions have been idle for `idle_grace_secs` |
| `on_demand` | only when you run `ocl task run <id>` |

### 8.2 Sorting and editing

```bash
ocl task ls
#   ID STATUS     LABEL       RANK  TITLE
#    2 running    urgent         2  Prod: 500s on /checkout since 14:00
#    1 queued     when_free      1  Upgrade pydantic to v3 and fix breakages
#    4 queued     when_free      4  Add pagination to /orders
#    3 queued     on_demand      3  Write release notes for 2.4

ocl task order 4 1                  # 4 first, then 1, then everything else as before
ocl task edit 1 --label urgent      # promote
ocl task edit 4 --body "Cursor-based, default page size 50"
ocl task run 3                      # start an on_demand task now
ocl task cancel 1                   # stops its coordinator and every agent it spawned
ocl task rm 3                       # delete (not while running)
```

### 8.3 What happens when a task starts

1. A coordinator session starts (`task #2 coordinator`) in a shared workspace `task-2`, with the task as its prompt.
2. It plans with todos and starts agents with `cluster_spawn_agent` (e.g. coder with `share_workspace: true`).
3. Whenever one of its agents finishes a turn, the coordinator gets a message: *"Session s-… (coder, "fix checkout") finished a turn. Todos 3/3 completed. Output: …"*.
4. It reviews, delegates more or replies, and finally calls `cluster_task_complete("summary")`. The task becomes `done`, the summary is stored as its result, and all its sessions stop.
5. If it stops without finishing, it is nudged up to `tasks.max_nudges` times, then the task is marked `failed`.

Watch it live:

```bash
ocl task show 2 --json | jq '{status, coordinator_session, notes, result}'
ocl events -f | grep -E 'task\.|turn_ended|spawn'
ocl sessions          # children show the coordinator as their parent (in --json)
```

Steer a running task the same way as any session:

```bash
ocl send <coordinator-session> "Don't touch the payments module; open a TODO instead."
```

### 8.4 Tuning the scheduler (`supervisor.yaml`)

```yaml
tasks:
  coordinator_agent: coordinator
  coordinator_node: hub          # optional pin
  max_concurrent_tasks: 2
  idle_grace_secs: 120           # how quiet the cluster must be before when_free work starts
  max_nudges: 2
  enabled: true                  # false = nothing starts unless you `ocl task run`
```

---

## 9. Workspaces and handing files between agents

| Setting | Where files live (on the node) | Who sees them |
|---|---|---|
| `workspace: {mode: isolated}` (default) | `data/workspaces/<session>/` | that session only |
| `workspace: {mode: shared, name: X}` | `data/workspaces/shared-X/` | every session on that node with the same name |
| workflow runs | `shared-run-<id>/` | all steps of the run |
| tasks | `shared-task-<id>/` | the coordinator, plus agents it spawns with `share_workspace: true` |

```bash
# Two agents collaborating on one tree
ocl spawn coder    --shared payments -p "Implement refunds in payments/refund.py"
ocl spawn reviewer-fast --shared payments -p "Wait for a message, then review payments/refund.py"

# Seed a shared workspace with an existing repo, then point agents at it
git clone https://example.org/me/payments ~/.opencode-cluster/data/workspaces/shared-payments
# (rootful podman: sudo chown -R 1000:1000 that directory)

# Grab results from the host side; no podman cp needed
ls ~/.opencode-cluster/data/workspaces/shared-task-2/
```

Shared workspaces have no locking. Tell agents (or the coordinator) who owns which files.

---

## 10. Budgets, limits and egress routes

Per agent:

```yaml
limits:
  max_sessions: 2          # concurrent sessions of this agent per node
  budget_usd: 1.00         # per session; keyproxy then answers "[opencode-cluster] session budget $1.00 exhausted"
  max_tokens: 500000       # per session
  wall_clock_secs: 1800    # aborts the session's work after 30 min
container:
  memory: 1g               # podman --memory
  cpus: 1.0
  pids: 512
  egress: none             # none = no network at all (recommended); full = normal networking
```

Per node: `max_sessions` in `node.yaml`. Budget for about 420 MB of RAM per session.

Costs need prices if your provider doesn't report them:

```yaml
# providers.yaml
providers:
  openrouter:
    base_url: https://openrouter.ai/api/v1
    prices: { qwen/qwen3-coder: { input_per_mtok: 0.20, output_per_mtok: 0.80 } }
```

### Make the provider see a different IP

```bash
# On the hub: a SOCKS tunnel through your VPS
ssh -f -N -D 127.0.0.1:1080 me@vps.example.org
```

```yaml
# routes.yaml
routes:
  via-vps: { type: proxy, url: "socks5h://127.0.0.1:1080" }
```

```yaml
# agents/coder.yaml
route: via-vps
```

Or set `default_route: via-vps` in `node.yaml` for every agent on that node. Only LLM calls take the route; agents still have no network.

---

## 11. Monitoring: status, events, metrics

```bash
ocl status                       # nodes, sessions (state, todos, tokens, cost), tasks, totals
watch -n2 ocl status             # poor man's dashboard until the web UI (Phase 4)
ocl events --limit 100           # recent event log
ocl events -f                    # follow live
ocl events -f --session s-81c2…  # one session: tool calls, state changes, turn ends
ocl events --json | jq -c 'select(.kind=="run.verdict") | .data'
```

Event kinds you'll see:
- **Sessions:** `session.state`, `session.turn_ended`, `tool.called`, `tool.failed`, `permission.asked`
- **Workflow runs:** `run.started`, `run.step`, `run.nudge`, `run.verdict`, `run.finished`
- **Tasks:** `task.started`, `task.finished`
- **Nodes:** `node.attached`, `node.detached`, `node.agents`

### Prometheus

```yaml
# prometheus.yml
scrape_configs:
  - job_name: opencode-cluster
    authorization: { credentials_file: /etc/prometheus/ocl-token }   # a read-scope token (§13)
    static_configs: [ { targets: ["127.0.0.1:7700"] } ]
```

Metrics: `ocl_nodes_connected`, `ocl_sessions{state}`, `ocl_tokens_total{agent,kind}`, `ocl_cost_usd_total{agent}`, `ocl_tasks{status}`.

Example queries:
- `sum(ocl_cost_usd_total)`
- `sum by (agent) (rate(ocl_tokens_total[1h]))`

---

## 12. REST API cookbook (curl)

All endpoints are under `/api/v1` and need `Authorization: Bearer <token>`. Mutating calls need `operate`; tokens and the audit log need `admin`.

```bash
API=http://127.0.0.1:7700/api/v1
H="Authorization: Bearer $(cat ~/.opencode-cluster/cli-token)"

curl -s -H "$H" $API/overview | jq '.totals'
curl -s -H "$H" $API/nodes | jq '.[] | {name, connected, agents: [.agents[].name]}'
curl -s -H "$H" $API/agents
curl -s -H "$H" "$API/sessions?all=true"
curl -s -H "$H" $API/sessions/s-81c2d0a3f4 | jq '{state, todos, tokens}'

# Spawn / message / interrupt / stop
curl -s -H "$H" -H 'content-type: application/json' -X POST $API/sessions \
  -d '{"agent":"coder","prompt":"Add a health endpoint","title":"health","node":null,"workspace":{"mode":"shared","name":"api"}}'
curl -s -H "$H" -H 'content-type: application/json' -X POST $API/sessions/s-81c2d0a3f4/message \
  -d '{"text":"Use FastAPI, not Flask","delivery":"queue"}'      # queue | steer | inbox
curl -s -H "$H" -X POST $API/sessions/s-81c2d0a3f4/interrupt
curl -s -H "$H" -H 'content-type: application/json' -X POST $API/sessions/s-81c2d0a3f4/permissions/per_7f2c \
  -d '{"reply":"once"}'
curl -s -H "$H" -X DELETE "$API/sessions/s-81c2d0a3f4?remove_workspace=false"

# Tasks
curl -s -H "$H" -H 'content-type: application/json' -X POST $API/tasks \
  -d '{"title":"Fix flaky test","body":"tests/test_sync.py fails 1 in 10","label":"urgent","tags":["ci"]}'
curl -s -H "$H" -H 'content-type: application/json' -X PATCH $API/tasks/7 -d '{"label":"when_free","rank":0.5}'
curl -s -H "$H" -H 'content-type: application/json' -X POST $API/tasks/reorder -d '{"ids":[7,3,5]}'
curl -s -H "$H" -X POST $API/tasks/7/run
curl -s -H "$H" -X POST $API/tasks/7/cancel
curl -s -H "$H" -X DELETE $API/tasks/7

# Workflows
curl -s -H "$H" $API/workflows | jq '.[].name'
curl -s -H "$H" -H 'content-type: application/json' -X POST $API/workflows/build-review/run -d '{"input":"slugify()"}'
curl -s -H "$H" $API/runs | jq '.[0] | {id, status, steps: [.steps[] | {step, verdict}]}'
curl -s -H "$H" -X POST $API/runs/r-1a6a0a0485/cancel

# Event log (paged) and live stream (SSE)
curl -s -H "$H" "$API/events?after=0&limit=200&session=s-81c2d0a3f4"
curl -sN -H "$H" $API/stream          # data: {"type":"session"|"task"|"run"|"node"|"log", …}
curl -sN "$API/stream?token=$(cat ~/.opencode-cluster/cli-token)"   # for EventSource clients

# Metrics, health
curl -s -H "$H" http://127.0.0.1:7700/metrics
curl -s http://127.0.0.1:7700/healthz                              # no auth
```

Errors come back as `{"error": "…"}` with 400, 401, 403 or 404.

---

## 13. Access tokens and the audit log

```bash
ocl token create grafana --scope read       # dashboards, Prometheus
ocl token create ci-bot --scope operate     # scripts that add tasks / spawn sessions
ocl token create backup-admin --scope admin
ocl token ls
ocl token revoke ci-bot

# Offline (supervisor stopped), e.g. if you lost the admin token:
ocl-supervisor token create rescue --scope admin
```

Each token is shown once and stored only as a hash.

Every control action is in a hash-chained audit log. The actor is `operator:<token>`, `session:<id>` for agent tool calls, or `system`.

```bash
ocl audit --limit 20
# 22:00:13 operator:admin   session.spawn   s-e3a64da210  ok
# 22:00:48 system           supervisor.stop -             ok
# 22:02:25 operator:admin   session.spawn   -             FAILED
ocl audit --verify            # {"ok": true, "entries": 66}
ocl-supervisor audit-verify   # same, offline
```

---

## 14. Scripting recipes

**Wait for a workflow run and fail CI if it didn't pass**

```bash
RUN=$(ocl workflow run build-review "$REQUEST" --json | jq -r .id)
while :; do
  ST=$(ocl workflow show "$RUN" --json | jq -r .status)
  [ "$ST" != running ] && break; sleep 10
done
echo "run $RUN: $ST"; [ "$ST" = passed ]
```

**Nightly maintenance tasks (cron)**

```cron
0 1 * * *  OCL_TOKEN=ocl_… ocl task add "Update dependencies, run tests, summarise breakages" --label when_free
30 1 * * 1 OCL_TOKEN=ocl_… ocl task add "Weekly: triage TODO comments into issues.md" --label when_free -t weekly
```

**Turn a TODO file into tasks**

```bash
grep -E '^- \[ \]' TODO.md | sed 's/^- \[ \] //' | while read -r line; do
  ocl task add "$line" --label when_free
done
```

**Spend report per agent (today)**

```bash
ocl sessions --all --json | jq -r --argjson since $(( $(date -d today +%s) * 1000 )) '
  map(select(.started_at >= $since)) | group_by(.agent)[]
  | "\(.[0].agent)\t\(map(.tokens.input + .tokens.output) | add) tok\t$\(map(.tokens.cost_usd) | add)"'
```

**Stop every idle session older than an hour**

```bash
ocl sessions --json | jq -r --argjson cut $(( ($(date +%s) - 3600) * 1000 )) \
  '.[] | select(.state=="idle" and .updated_at < $cut) | .id' | xargs -rn1 ocl stop
```

**Alert when something gets blocked**

```bash
ocl events -f --json | jq --unbuffered -c 'select(.kind=="permission.asked")' |
while read -r ev; do notify-send "agent needs permission" "$ev"; done
```

---

## 15. Running as systemd services

```ini
# /etc/systemd/system/ocl-supervisor.service
[Unit]
Description=opencode-cluster supervisor
After=network-online.target

[Service]
User=ocl
Environment=OCL_CONFIG_DIR=/home/ocl/.opencode-cluster
ExecStart=/usr/local/bin/ocl-supervisor run
Restart=on-failure
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/home/ocl/.opencode-cluster /run/ocl
RuntimeDirectory=ocl
RuntimeDirectoryPreserve=yes

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/opencode-node.service
[Unit]
Description=opencode-cluster node
After=ocl-supervisor.service

[Service]
User=ocl
Environment=OCL_CONFIG_DIR=/home/ocl/.opencode-cluster
Environment=OCL_BRIDGE=/usr/local/bin/ocl-bridge
# Provider keys as encrypted credentials: referenced in agents as systemd-creds:<name>
LoadCredentialEncrypted=openrouter:/etc/ocl/creds/openrouter.cred
LoadCredentialEncrypted=groq:/etc/ocl/creds/groq.cred
ExecStart=/usr/local/bin/opencode-node run
Restart=on-failure
# Containers keep running across restarts of this unit; it re-attaches.
KillMode=process

[Install]
WantedBy=multi-user.target
```

```bash
# create an encrypted credential (never stored in plain text)
sudo systemd-creds encrypt --name=openrouter - /etc/ocl/creds/openrouter.cred   # paste key, Ctrl-D
sudo loginctl enable-linger ocl     # rootless podman: keep the user's containers alive without a login
sudo systemctl daemon-reload && sudo systemctl enable --now ocl-supervisor opencode-node
journalctl -u opencode-node -o cat | jq -r '.fields.message'      # logs are JSON
```

`OCL_LOG=debug` (or e.g. `OCL_LOG=info,opencode_node::manager=debug`) raises verbosity.

---

## 16. Testing offline with the mock LLM

`examples/mock-llm/mock_llm.py` is an OpenAI-compatible server that replays scripts. The model name selects the script, so the `builder` model uses `scripts/builder.json`. It's great for testing workflows, permissions and the task bucket without spending tokens.

```bash
python3 examples/mock-llm/mock_llm.py --port 8099 --require-key test-key &
export MOCK_API_KEY=test-key     # for opencode-node; proves keyproxy injects the real key
```

```json
// examples/mock-llm/scripts/linter.json — a simple script: one step per model call
[
  {"tool": "todowrite", "args": {"todos": [{"content": "run linter", "status": "in_progress", "priority": "high"}]}},
  {"tool": "bash", "args": {"command": "python3 -m pyflakes . || true", "description": "lint"}},
  {"tool": "todowrite", "args": {"todos": [{"content": "run linter", "status": "completed", "priority": "high"}]}},
  {"delay": 2, "text": "Lint finished. VERDICT: APPROVED"}
]
```

Phase scripts react to the latest message (see `coordinator.json`):

```json
{"phases": [
  {"match": "(reviewer", "steps": [ … ]},
  {"match": "(builder",  "steps": [ … ]},
  {"steps": [ … default … ]}
]}
```

Point an agent at it with `provider: { id: mock, model: linter, key: { ref: "env:MOCK_API_KEY" } }`. The example `providers.yaml` defines `mock`. Use OpenCode's v1 tool argument names (`filePath`, `oldString`/`newString`, `command` + `description`). `GET http://127.0.0.1:8099/v1/_log` shows recent requests.

The whole demo is `examples/quickstart/run-local.sh --with-worker [--keep]`.

---

## 17. Recovery and maintenance

| Situation | What to do | What happens |
|---|---|---|
| Supervisor restarts | Nothing. | Nodes reconnect with backoff and replay buffered events; running sessions are reconciled. |
| Node daemon restarts | Nothing. | It re-attaches to its running containers, restarts stopped ones, and resyncs each session from OpenCode's history. |
| Machine reboot | Start both services. | Stopped containers are restarted and sessions continue from their saved OpenCode state. |
| Session shows `unreachable` | Check the node. | After `disconnect_grace_secs` (default 600) it becomes `lost`. |
| Session shows `lost` | Spawn a new one; the workspace is still on disk. | — |
| Stuck in `starting` > 2 min | `podman logs ocl-<session>`; OpenCode log under `data/instances/<session>/home/.local/share/opencode/log/` | — |
| `[opencode-cluster] …` error in a session | It came from keyproxy: budget, token cap, model allowlist or missing secret. Check `opencode-node check`. | — |

Where data lives (default `~/.opencode-cluster`):

```
supervisor/supervisor.db      sessions, tasks, runs, events, audit, API token hashes (back this up)
data/node.db                  node outbox + instance index
data/workspaces/              all workspaces
data/instances/<session>/     generated opencode config (capability only) + OpenCode home
cli-token                     admin token from `init` (0600) — move it somewhere safe
```

Clean-up:

```bash
podman ps -a --filter label=ocl.session            # every agent container
ocl sessions --json | jq -r '.[] | select(.state=="idle") | .id' | xargs -rn1 ocl stop
sqlite3 ~/.opencode-cluster/supervisor/supervisor.db "DELETE FROM events WHERE ts < strftime('%s','now','-30 days')*1000"
```

Upgrading OpenCode: bump `OPENCODE_VERSION` in `images/agent/Containerfile`, rebuild the image, regenerate `node-daemon/tests/fixtures/opencode-<ver>-paths.json` from the new server's `/doc`, and run `cargo test`. The test fails if an endpoint we rely on disappeared.

---

## 18. What's coming

| Feature | Phase |
|---|---|
| Remote nodes over mTLS; **add a machine from the web UI with a one-time join token**; `via_node` egress routes | 2 |
| Artifact store (`cluster_artifact_put/get`), cross-machine shared workspaces, lineage | 3 |
| Web UI (SolidJS, mobile-first: live sessions, drag-to-sort task board, enrollment), TUI, password login, built-in TLS | 4 |
| Load test (~10 sessions), pre-warmed containers, Quadlet units, event export, keyproxy usage metering | 5 |
