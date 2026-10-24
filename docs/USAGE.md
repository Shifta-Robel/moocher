# opencode-cluster — Usage guide

Practical, copy-pasteable examples for every feature that exists today (Phases 1–4: hub, local and remote nodes, relays, quotas, templates, artifacts, questions, recovery from dead agents, harvesting, the web dashboard with login and HTTPS, Open in OpenCode, notifications, the TUI). Features from later phases are listed at the end. Enrolling machines and debugging them: [RUNBOOK.md](./RUNBOOK.md).

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
18. [Templates from the hub](#18-templates-from-the-hub)
19. [Questions and escalation](#19-questions-and-escalation)
20. [Artifacts](#20-artifacts)
21. [When an agent dies](#21-when-an-agent-dies)
22. [Outputs, harvesting and clean-up](#22-outputs-harvesting-and-clean-up)
23. [The web dashboard and login](#23-the-web-dashboard-and-login)
24. [HTTPS on your domain](#24-https-on-your-domain)
25. [Open in OpenCode](#25-open-in-opencode)
26. [Notifications: phone push, ntfy, webhooks](#26-notifications-phone-push-ntfy-webhooks)
27. [The terminal dashboard](#27-the-terminal-dashboard)
28. [What's coming](#28-whats-coming)

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

Session states: `starting → idle ⇄ running`, plus `blocked` (waiting on a permission or a question), `parked` (waiting for a provider allowance), `error`, `stopped`, `unreachable` (node disconnected), `lost` (node gone), `dead` (its container died; volumes kept) and `halted` (paused with its container kept, after another agent died).

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

Each permission request is also a **question** (§19): it goes to the agent that started the session first, then up the chain, and to you depending on the question mode. `ocl questions` shows it; `ocl answer <q-id> once|always|reject` answers it. Requests matching the vital rules (`rm -rf`, `deploy`, …) are `vital`.

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

**Across machines.** A shared workspace can be used by sessions on different nodes; each node has its own copy and the hub moves changes between them through the artifact store:
- before a session starts on a node whose copy may be behind (it shows in `ocl show`: `placed: … workspace demo: synced from hub`);
- after every turn, towards the other machines that have live sessions in it;
- when an agent calls `cluster_sync_workspace`.

Each sync is a file-by-file three-way merge against the last state both sides saw. A file changed on only one side takes that side's version (added, updated or deleted). A file changed on **both** sides keeps the local version, and the incoming one is written next to it as `<file>.theirs`; the agents in that copy are told which files to reconcile. Workflow runs use this too: a step can run on another machine than the previous one (`node:` per step, or placement), and the run's workspace follows it.

**Handing over files without sharing a workspace:** artifacts (§20).

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

### Out through another machine of the cluster (`via_node`)

```yaml
# routes.yaml on the worker
routes:
  via-relay-de: { type: via_node, node: relay-de }   # a node with role: relay (or relay: true)
```

The relay connects to the provider from its own IP. TLS stays end to end, and the key stays on the worker. Enrolling a relay is described in RUNBOOK §2.2.

### Switch IP when a free allowance runs out

Keyless gateways count per IP. Give the agent a plan, and tell the node how the provider counts:

```yaml
# agents/free-coder.yaml
provider: { id: kilo-free, model: auto }
route: { prefer: direct, failover: [via-relay-de, via-vps], on_exhausted: wait }

# providers.yaml
providers:
  kilo-free:
    quota:
      scope: ip
      limits: [ { kind: requests, max: 200, window: 1d, at: "00:00", tz: UTC } ]
```

```bash
ocl quotas kilo-free          # per-IP buckets: used/limit, left, RESETS IN 3h 24m, source (header/429/config/override)
ocl ips                       # which nodes and sessions share each IP
ocl quota set kilo-free --scope-id 203.0.113.7 --resets-at 03:15 --remaining 0   # you know better than the counters
ocl route set vps-fi free-coder --prefer via-relay-de --failover direct          # live, kept in that node's overrides.yaml
```

When every route is used up, the session is `parked`, and `ocl status` shows the countdown. It resumes by itself after the reset. For BYOK accounts use `scope: account` (keys labelled with the same `account`) or `scope: key`. Their limits and resets are learned from the provider's rate-limit headers.

---

## 11. Monitoring: status, events, metrics

```bash
ocl status                       # nodes, sessions (state, todos, tokens, cost), tasks, totals
ocl tui                          # dashboard in the terminal (or the web dashboard, §23)
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
| Session shows `unreachable` | Check the node. | After `disconnect_grace_secs` (default 600) it becomes `lost`, which counts as a death: see §21. |
| Session shows `lost` or `dead` | `ocl halts`, then `ocl recover <halt> restart|resume_elsewhere|handoff|abandon`. | The cheapest recovery may already have run by itself (§21). |
| Task shows `halted` | `ocl halts` shows the options with their costs; `ocl recover <h-id> <option>` resumes everything. | Halted sessions keep their containers until then. |
| Stuck in `starting` > 2 min | `podman logs ocl-<session>`; OpenCode log under `data/instances/<session>/home/.local/share/opencode/log/` | — |
| `[opencode-cluster] …` error in a session | It came from keyproxy: budget, token cap, model allowlist or missing secret. Check `opencode-node check`. | — |

Where data lives (default `~/.opencode-cluster`):

```
supervisor/supervisor.db      sessions, tasks, runs, events, audit, API token hashes (back this up)
data/node.db                  node outbox + instance index
supervisor/artifacts/b3/       artifact blobs (content-addressed; back this up too)
data/workspaces/              all workspaces
data/instances/<session>/     generated opencode config (capability only) + OpenCode home (kept for `retain:`)
data/cas/                     this node's artifact cache
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

## 18. Templates from the hub

Put agent templates in the hub's `templates/`; every worker node receives them. A template names a secret *alias*, which each node maps to its own secret:

```yaml
# hub: templates/coder.yaml
name: coder
provider: { id: openrouter, model: qwen/qwen3-coder, key: { secret: openrouter } }
instances: { max: 4 }
overridable: { role_append: true, budget_usd: 3.00, routes: [direct, via-relay-de] }

# each worker's node.yaml
secrets:
  openrouter: { ref: "systemd-creds:openrouter", account: personal }
```

```bash
ocl templates                                   # where each template can run, and why not elsewhere
ocl spawn coder --append "Focus on the parser" --budget 2 --route via-relay-de
ocl show <session> --preamble                   # the rendered instructions it started with
```

Coordinators get the same through `cluster_spawn_agent` (`role_append`, `budget_usd`, `route`), and `cluster_list_nodes` lets them choose a machine.

## 19. Questions and escalation

Agents ask in two ways:
- **`cluster_ask(question, options?, importance, context)`**. It returns at once; the answer arrives as a message.
- **OpenCode's own `question` tool.** The session waits for the reply; the node hands the question to the cluster.

`ask` permissions (§6) and budget stops are questions too.

**Where a question goes:**
1. To the asker's **parent** (the agent that started it). The parent answers with `cluster_answer(id, answer)` or passes it up with `cluster_escalate(id, note)`; the note travels with the question.
2. Up the chain, one level at a time. A holder that does nothing for `answer_timeout_secs` (default 300) passes it up automatically.
3. At the top, the **question mode** decides whether it reaches you. If not, the top agent decides and its decision goes to the **assumption log**.

| Mode | What reaches you |
|---|---|
| `ask_me_everything` | Every question, at once. The parent sees an FYI in its inbox but does not answer. |
| `important_and_vital` *(default)* | `normal` and `vital` questions the chain could not answer. `low` stays in the chain. |
| `vital_only` | Only `vital` questions, after the chain fails. |
| `autonomous` | Only what no agent may decide (credentials, budgets). |

Importance is set by the asker; **rules only raise it**. Mentions of money, credentials, deleting data, irreversible or external actions make it `vital` (`questions.vital_keywords`). Questions about passwords, secrets or keys are **operator-only**: no agent may answer them, whatever the mode.

```bash
ocl questions                          # open ones: importance, then how many wait on it, then age
# q-3f2a…  vital  open  ask  blocked 2  1m  on worker-a task #3
#     May I delete the old data directory?
#     chain: builder s-81c2… → s-77ab… → operator   held by operator
#     note (s-77ab…): the coordinator does not know whether it is still used
#     raised to vital: mentions "delete"
ocl answer q-3f2a… "Yes, it was migrated last week"
ocl answer q-3f2a… --choice 2          # pick an option
ocl answer q-3f2a… --decide            # "let the agent decide" (logged as an assumption)
ocl answer q-3f2a… --reject

ocl questions mode                     # cluster mode and per-task modes
ocl questions mode vital_only          # switch live
ocl questions mode autonomous --task 7 # or for one task (also: ocl task add … --question-mode …)
ocl assumptions [--task 7]             # decisions made without you, for review
```

**Budgets:** when keyproxy refuses a call because a session's `budget_usd` is used up, you get a `vital` operator-only question: `ocl answer q-… "raise 2.50"` raises it live, `stop` stops the session (its outputs are harvested).

**Upward reports:** `cluster_report_up(message)` sends a status report to the parent, or for a top-level agent, to the task notes and the notification centre (`ocl notifications`).

Questions are also in `GET /api/v1/questions` (and on the SSE stream), answered with `POST /api/v1/questions/{id}/answer`.

---

## 20. Artifacts

Content-addressed files and directory trees, stored on the hub (`art://b3/<hash>`). Identical content has the same reference. Metadata records who made it, on which node, for which task, and **lineage**: the artifacts it was made from.

```bash
ocl artifact put report.pdf --tag release
ocl artifact put ./build-output --name build-42      # a directory becomes a tree (tar.zst)
ocl artifact ls [text] [--task 3]
ocl artifact show art://b3/9f2c…                      # metadata, parents ("from") and children ("used by")
ocl artifact get art://b3/9f2c… -o out/               # a tree is unpacked into the directory
```

**Agents** use:
- `cluster_artifact_put(path)` returns a reference, which they can attach to `cluster_send_message(…, artifacts=[ref])`;
- `cluster_artifact_get(ref, path?)` puts it into their workspace;
- `cluster_artifact_list` and `cluster_artifact_info`.

Whatever an agent publishes lists the artifacts it fetched before as parents, so `ocl artifact show` traces where a result came from.

Bytes move on their own gRPC stream to and from the hub (never node to node). Each node caches blobs (`artifact_cache`, default 2 GB), so a second fetch on the same node does not download again. Links in a workspace are never followed or packed.

---

## 21. When an agent dies

An instance is **dead** when its container or process ends without being stopped. The reason is one of:
- `oom`;
- `exit:<code>` (137 is a kill);
- `wall_clock` (`limits.wall_clock_secs`);
- `node_lost` (its machine was unreachable for `disconnect_grace_secs`);
- `container_gone`.

Its volumes are kept. The hub prices each way back:

| Option | Estimated cost |
|---|---|
| `restart` | Same node, same OpenCode home, history intact. Re-reading the last context × the input price. Only if the node is up. |
| `resume_elsewhere` | A fresh instance of the same template (`--node` to choose). The workspace comes from a snapshot of the dead volume, or the latest checkpoint if the node is gone. It is briefed with the original prompt, todos and last output. Same estimate. |
| `handoff` | The parent is asked to start a replacement with its own summary. `handoff_factor` × spend so far. |
| `abandon` | The step or task fails with the death reason. |

The cheapest option runs **automatically** once (`on_death.auto_retry`), if it costs at most `$0.50` and at most 25% of what the task has spent (for $0 providers: at most 25% of the allowance left). The wall-clock limit and crash loops (3 deaths in 10 minutes) are never retried automatically. Otherwise the task (or workflow run, or the session tree) is **halted**:
1. every live instance in it is interrupted;
2. its outputs are harvested;
3. its container is stopped and kept.

You get a `vital` notification with the reason, what it was doing, the spend and the priced options:

```bash
ocl halts
# h-2e1c…  open  task:3  s-81c2… died (exit:137)  spent $0.4120  2m
#     was doing: write tests
#     halted: s-77ab…, s-90fe…
#     * restart           ~$0.0310   same node worker-a, history intact; re-reads ~62000 tokens of context
#     * resume_elsewhere  ~$0.0310   on hub (best placement); workspace: snapshot taken now from the dead instance's volume
#     * handoff           ~$0.2060   parent s-77ab… spawns a replacement with its own summary
#     * abandon           ~$0.0000   give up: the step/task fails with the death reason
ocl recover h-2e1c… restart                     # one call resumes the whole tree
ocl recover h-2e1c… resume_elsewhere --node hub
ocl recover s-81c2… restart                     # a dead session without a halt
```

**Checkpoints:** every `checkpoint_every_turns` (default 3) finished turns, a node sends a snapshot of the workspace and the transcript to the hub. If a machine is lost, the replacement starts from there. The harvest record says that anything written after the checkpoint is unrecoverable. If the lost machine comes back later, its old instance of a replaced session is stopped.

---

## 22. Outputs, harvesting and clean-up

Templates name the files worth keeping (`outputs:` in CONFIG.md). They are collected when an instance is stopped, dies, is halted, reaches its wall clock, or its task ends:

```yaml
outputs:
  - { path: report.md, to: parent_workspace, dest: from-builder/report.md }
  - { path: "dist/*.whl", to: artifact, name: wheels }
transcript: true
retain: on_failure
```

```bash
ocl harvests --session s-81c2…
# 14:02:11 s-81c2… on worker-a (stop)
#     report.md      art://b3/77e1…   812B → parent_workspace isolated:s-77ab… on hub: from-builder/report.md
#     dist/*.whl     art://b3/1c4d…  2048B → artifact
#     missing.txt    (matched nothing)
#     transcript art://b3/a09b…
```

The lifecycle is `running → stopping → harvesting → stopped → reaped`. After the harvest, the capability token stops working (the instance's gateway is gone). `retain:` decides when the OpenCode home and isolated workspace are removed (default 24 h); `ocl stop --rm-workspace` removes them at once.

---

## 23. The web dashboard and login

The supervisor serves the dashboard itself (no separate web server): open `http://127.0.0.1:7700` on the hub, or your domain once HTTPS is set up (§24).

```bash
ocl-supervisor user set admin                 # asks for a password (≥ 10 characters)
ocl-supervisor user set alice --scope operate # read | operate | admin, as for API tokens
ocl-supervisor user totp admin                # authenticator-app codes (prints the secret / otpauth:// URI)
ocl-supervisor user list
```

**Views:**
- **Overview:** a status strip (nodes, providers waiting for a reset, questions, halts) and live instances grouped by node.
- **Instances:** filter, and group by node, egress IP, template, task or status. Each instance has:
  - tokens, cost, CPU, memory and disk;
  - todos, activity, the chain, *what it was told*, placement, outputs and recovery;
  - a message box (queue or steer);
  - Interrupt, Stop and **Open in OpenCode**.
- **Questions:** the inbox (§19) with one-click answers, the question mode, history and the assumption log.
- **Tasks:** the bucket, drag to reorder (or ▲/▼ on a phone), halts with priced recovery buttons, workflows and runs.
- **Quotas:** allowances with reset countdowns and overrides, plus egress IPs.
- **Templates:** where each can run, and **New instance** with the overrides the template allows.
- **Nodes & proxies:**
  - host load, memory and disk;
  - **Add machine** (shows the join command);
  - approve, revoke and forget;
  - the allowed source IPs;
  - live route changes per agent, and **Write to YAML** to make one permanent.
- **Artifacts:** search, lineage, upload and download.
- **Notifications, Audit and Settings:** theme, password, users and push.

Statuses always show a colour, an icon and a word: running ● (pulsing), idle, waiting ?, parked ◷, error, dead ✗ on a striped card, halted, stopped. Each node has a stable colour, used for the card's edge and its chip. Dark mode follows the system, or the toggle (remembered). On a phone the navigation is a bottom tab bar. The dashboard is a PWA ("Add to Home Screen").

**Logging in:**
- **Session:** a browser session lasts `login.session_hours` (12 h).
- **Lockouts:** five failed logins in 15 minutes lock that account for 15 min, doubling each time (max 24 h). Twenty from one IP block that IP. A lockout is a vital notification, and every login, failure and logout is audited.
- **Protection:** changes from the browser need the CSRF token the UI sends automatically.
- **Scripts:** API tokens (`Authorization: Bearer`) keep working.

---

## 24. HTTPS on your domain

Point your domain's DNS record at the hub (the only DNS needed), open ports 80 and 443, then:

```yaml
# supervisor.yaml
api_listen: 0.0.0.0:443
tls:
  acme:
    domains: [ocl.example.org]
    email: you@example.org
    http01_listen: 0.0.0.0:80      # challenges; everything else redirects to https
attach_listen: 0.0.0.0:7444        # Open in OpenCode (§25), same certificate
```

- **First start:** the hub orders the certificate from Let's Encrypt. To try it first, set `directory: https://acme-staging-v02.api.letsencrypt.org/directory`.
- **Storage:** the certificate is kept in `<data>/acme/` (keys 0600).
- **Renewal:** it renews 30 days before expiry and is swapped in without a restart.
- **Ports below 1024:** binding them needs `CAP_NET_BIND_SERVICE` (`AmbientCapabilities=CAP_NET_BIND_SERVICE` in the unit) or root.

**Your own certificate:** `tls: { cert_file: /etc/ocl/fullchain.pem, key_file: /etc/ocl/key.pem }`. Replacing the files is picked up within a minute. It must be a server certificate: a self-signed certificate that is its own CA is rejected by rustls-based clients. Use a small private CA or mkcert, and give the CLI `OCL_CA_CERT=/path/ca.pem`.

**Behind Caddy or nginx:** keep `api_listen` on loopback, set `trusted_proxies: [127.0.0.1]` (so lockouts see the real client IP and cookies get `Secure`), and proxy both the dashboard port and the attach port.

`ocl` talks to HTTPS with `OCL_URL=https://ocl.example.org`.

---

## 25. Open in OpenCode

**Open in OpenCode** on an instance page opens OpenCode's own web app for that instance, in a new tab. That includes its terminal, file view and session history.

- **Where:** it is served from the hub's `attach_listen` port, a separate origin that OpenCode needs.
- **How access works:**
  1. The hub swaps a one-time, 60-second ticket for an 8-hour cookie.
  2. The node checks that cookie on **every** request.
  3. The node adds the container's OpenCode password itself; your browser never sees it.
- **Bytes:** they travel over the node's existing link, so nodes need no open ports.
- **One instance per browser:** opening another instance switches to it.
- **Marking:** the instance shows *attached by …*, and the attach is audited.

In a terminal:

```bash
ocl attach s-81c2…                  # runs `opencode attach` against a local proxy that adds the cookie
ocl attach s-81c2… --no-launch      # just prints the local address (e.g. for a second terminal)
```

---

## 26. Notifications: phone push, ntfy, webhooks

Everything appears in the notification centre (bell, with an unread count). What also goes **out** is decided by rules. By default `warning` and `vital` go to every configured channel, and `info` stays in the dashboard. During quiet hours only `vital` goes out.

- **Phone push (Web Push):**
  1. Open **Notifications → Enable push on this device** (needs HTTPS).
  2. On an iPhone, first use "Add to Home Screen".
  3. Push works with the tab closed. Vital ones stay on screen until you react, and tapping one opens the right page.
- **ntfy:**
  ```yaml
  notifications:
    ntfy: { url: https://ntfy.sh/my-private-topic, token: "env:NTFY_TOKEN" }
  ```
- **Webhook:** set `webhook: { url: …, secret: "file:/etc/ocl/hook" }`. It POSTs JSON `{title, body, kind, severity, target, id, ts}`, signed with `X-OCL-Signature: sha256=…`.
- **Rules:** `rules: [{ kinds: ["question.*", "task.halted"], min_severity: info, channels: [push] }]`. Event kinds are listed in CONFIG.md.
- **Test:** **Send a test** on the Notifications page, or `curl -X POST …/api/v1/notifications/test`.

What triggers them:
- questions waiting for you;
- halts, and dead agents recovered automatically;
- tasks and runs finishing;
- nodes going offline or coming back;
- an allowance used up or reset;
- a session at 80% of its budget;
- a provider refusing its key, or unreachable;
- disk at 90%;
- login lockouts;
- certificate problems.

---

## 27. The terminal dashboard

```bash
ocl tui
```

- **Views:** instances, questions, halts with tasks, and nodes. Switch with `1`–`4` or Tab.
- **Keys:** ↑/↓ select, Enter shows details, `a` answers a question, `m` messages an instance, `x` stops it, `r` recovers a halt (cheapest option), `q` quits.
- **Refresh:** every 2 s.
- **Remote use:** it works over SSH with the same API token as `ocl`.

---

## 28. What's coming

| Feature | Phase |
|---|---|
| ~~Remote nodes over mTLS, join tokens, `via_node` routes, quotas and failover, templates~~ | 2 ✔ (web UI for these: 4) |
| ~~Artifact store, cross-machine shared workspaces, lineage, questions and escalation, dead-agent recovery and halts, harvesting~~ | 3 ✔ (UI views and push notifications for these: 4) |
| ~~Web UI (SolidJS, mobile-first), TUI, password login, built-in TLS, Open in OpenCode, notifications~~ | 4 ✔ |
| Load test (~10 sessions), pre-warmed containers, Quadlet units, event export, keyproxy usage metering | 5 |
