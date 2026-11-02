# moocher — Usage guide

Practical, copy-pasteable examples for every feature that exists today (Phases 1–5: hub, local and remote nodes, relays, quotas, templates, artifacts, questions, recovery from dead agents, harvesting, the web dashboard with login and HTTPS, Open in OpenCode, notifications, the TUI, usage metering, egress allowlists, housekeeping, hard disk limits, systemd units). Features from later phases are listed at the end. Enrolling machines and debugging them: [RUNBOOK.md](./RUNBOOK.md).

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
28. [Usage metering](#28-usage-metering)
29. [Network allowlists](#29-network-allowlists-egress-allowlist)
30. [Housekeeping: event export and artifact expiry](#30-housekeeping-event-export-and-artifact-expiry)
31. [Hard disk limits](#31-hard-disk-limits)
32. [What's coming](#32-whats-coming)

---

## 1. Mental model in 60 seconds

```
you ──moo / curl──▶ moocher-hub (hub) ◀──gRPC── moocher-node (hub)  ──▶ podman: [moo-bridge + opencode] × N
                                          ◀──gRPC── moocher-node (more)  ──▶ …
```

| Term | Meaning |
|---|---|
| **Agent (profile)** | A YAML file in `agents/`: model, key reference, role prompt, tool permissions, limits. It is a template. |
| **Session** | One running OpenCode conversation spawned from a profile, in its own container. It has an id like `s-3f9a1c2b7d`. |
| **Node** | A machine running `moocher-node`. The hub is a node too. |
| **Workflow** | A fixed sequence of steps (build → review), each a session, with pass/fail rules. |
| **Task** | An item in your bucket. A *coordinator* session (the main agent) does it using other agents. |
| **Todos** | Every agent keeps an OpenCode todo list. Its progress is how success is judged. |
| **Capability token** | What an agent holds instead of an API key. It only works through its own node, for its own model, within its budget. |

Session states: `starting → idle ⇄ running`, plus `blocked` (waiting on a permission or a question), `parked` (waiting for a provider allowance), `error`, `stopped`, `unreachable` (node disconnected), `lost` (node gone), `dead` (its container died; volumes kept) and `halted` (paused with its container kept, after another agent died).

---

## 2. One-time setup

```bash
git clone <this repo> moocher && cd moocher
rustup target add x86_64-unknown-linux-musl
cargo build --release
cargo build --release -p moo-bridge --target x86_64-unknown-linux-musl
podman build -t localhost/moocher-agent:1.19.4 -f images/agent/Containerfile .

# Put binaries on PATH (or use target/release/… directly)
sudo install -m755 target/release/{moo,moocher-hub,moocher-node} /usr/local/bin/
sudo install -m755 target/x86_64-unknown-linux-musl/release/moo-bridge /usr/local/bin/

# Start from the example config
mkdir -p ~/.moocher && cp -r examples/quickstart/hub/* ~/.moocher/
```

The hub's config directory now holds both halves:

```
~/.moocher/
  supervisor.yaml   node.yaml   providers.yaml
  agents/builder.yaml  agents/reviewer.yaml  agents/coordinator.yaml
  prompts/…            workflows/build-review.yaml
```

Initialise and start:

```bash
moocher-hub init          # prints an admin token once; also saved to ~/.moocher/cli-token (0600)
moocher-hub run &         # API on 127.0.0.1:7700, node socket /run/moo/supervisor.sock
moocher-node check          # validates agents, resolves secrets (never prints them)
moocher-node run &          # the hub's own node
moo agents                   # READY=yes for every agent you expect
```

`moo` reads the token from `MOO_TOKEN`, or else `~/.moocher/cli-token`, and the server from `MOO_URL` (default `http://127.0.0.1:7700`). Add `--json` to any command for machine-readable output.

> If `/run` isn't writable for your user, set `runtime.run_dir` in `node.yaml` and `link_socket` in `supervisor.yaml` to e.g. `$XDG_RUNTIME_DIR/moo`, and point `node.yaml: supervisor:` at the same socket.

### A second node on the same machine

Useful for testing placement, or for keeping two sets of keys apart:

```bash
mkdir -p ~/moo-worker-a/agents
cp ~/.moocher/providers.yaml ~/moo-worker-a/ && cp -r ~/.moocher/prompts ~/moo-worker-a/
cp ~/.moocher/agents/builder.yaml ~/moo-worker-a/agents/
cat > ~/moo-worker-a/node.yaml <<'EOF'
name: worker-a
supervisor: unix:///run/moo/supervisor.sock
max_sessions: 4
runtime: { run_dir: /run/moo-worker-a }
EOF
moocher-node --config-dir ~/moo-worker-a run &
moo status        # two nodes, connected
```

---

## 3. Defining agents

One file per agent in `agents/`. Changes are picked up automatically: limits, keys and routes apply immediately, and role, tools and container settings apply to the next session. Run `moocher-node check` after editing.

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

`OPENROUTER_API_KEY` must be in **`moocher-node`'s** environment, not in your shell for `moo`. With systemd, use credentials instead (§15):

```yaml
  key: { ref: "systemd-creds:openrouter" }
```

### 3.2 Fast reviewer on Groq

```yaml
# agents/reviewer-fast.yaml
name: reviewer-fast
description: Quick code review. Ends with VERDICT APPROVED or CHANGES_REQUESTED.
provider: { id: groq, model: llama-4-maverick, key: { ref: "file:/etc/moo/keys/groq" } }
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

Keyless gateways are marked `best_effort` in `moo agents`. Expect rate limits.

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
moocher-node check
#   ✓ agent coder            openrouter/qwen/qwen3-coder
#   ✗ agent reviewer-fast    groq/llama-4-maverick  [secret does not resolve]
moo agents
moocher-node schema --out ~/.moocher/schemas   # JSON Schemas for your editor
```

---

## 4. Everyday session control

```bash
# Start a session (placed on any node that has the agent and capacity)
moo spawn coder -p "Add a --json flag to cli.py and tests for it" --title "json flag"
# s-81c2d0a3f4 on hub

moo spawn coder -p "…" --node worker-a          # pin to a node
moo spawn coder -p "…" --shared payments-api    # use a named shared workspace (§9)

moo sessions                 # live + recent (add --all for history)
moo show s-81c2d0a3f4        # state, model, workspace, tokens, cost, todos, last output
moo interrupt s-81c2d0a3f4   # abort the current step (session stays alive)
moo stop s-81c2d0a3f4        # stop and remove the container (workspace kept)
moo stop s-81c2d0a3f4 --rm-workspace   # isolated workspaces only
```

Example `moo show` output:

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
moo send s-81c2 "Also handle the empty-input case."             # queue (default)
moo send s-81c2 "Stop — use argparse, not click." --steer       # steer
moo send s-81c2 "FYI: CI is red on main, not your fault." --inbox
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
| `cluster_list_agents` | which agents can start now (`ready`), which wait for a provider allowance (`wait_allowance`, with `wait_s`), which are full (`busy`, with the idle sessions that could be stopped), and what the cluster has observed of each model (§11.1) |

Prompt pattern that works well:

```bash
moo spawn coder --title "api" -p "Implement the /orders endpoint. When done, send a
message to session s-REVIEWER with cluster_send_message summarising what changed."
```

---

## 6. Permissions and blocked sessions

Anything set to `ask` in an agent's `tools` pauses the session until you answer:

```bash
moo sessions
# s-81c2…  coder  hub  blocked  1/3  …  permission: bash ["rm -rf build"] (request per_7f…)

moo permit s-81c2d0a3f4 per_7f2c… once     # allow this one call
moo permit s-81c2d0a3f4 per_7f2c… always   # allow this pattern for the rest of the session
moo permit s-81c2d0a3f4 per_7f2c… reject   # the agent is told it was refused
```

Tip: use `ask` as the default for `bash`, with an explicit `allow` list of safe commands. Most sessions then never block.

Each permission request is also a **question** (§19): it goes to the agent that started the session first, then up the chain, and to you depending on the question mode. `moo questions` shows it; `moo answer <q-id> once|always|reject` answers it. Requests matching the vital rules (`rm -rf`, `deploy`, …) are `vital`.

---

## 7. Workflows (turn-based handoffs)

A workflow is a YAML file in the hub's `workflows/`. Each step spawns a session. When it goes idle the supervisor judges it, then hands off to the next step. All steps of a run share one workspace.

### 7.1 Run the bundled one

```bash
moo workflow ls
# build-review   Builder implements, reviewer approves or sends it back.  [build → review]

moo workflow run build-review "Write slugify(text) in slug.py with pytest tests"
# run r-1a6a0a0485 started

moo workflow runs
# r-1a6a0a0485  build-review  Running  40s  build:Pass → review:…
moo workflow show r-1a6a0a0485 --json | jq '.steps[] | {step, verdict, check_results}'
moo workflow cancel r-1a6a0a0485
```

### 7.2 A three-step plan → build → review with a fix-up loop

```yaml
# workflows/feature.yaml
apiVersion: moo/v1
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
moo workflow run feature "Rate-limit the /login endpoint to 5 attempts per minute per IP"
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
moo task add "Upgrade pydantic to v3 and fix breakages" --label when_free
moo task add "Prod: 500s on /checkout since 14:00" -b "Stack trace in logs/checkout.txt. Find and fix." --label urgent
moo task add "Write release notes for 2.4" --label on_demand
moo task add "Add pagination to /orders" -t workflow:feature --label when_free   # run a workflow instead
moo task add "Refresh docs" -t docs -t low-risk                                  # free-form tags
```

| Label | Starts… |
|---|---|
| `right_now` | at once; less urgent sessions are paused for room and resume afterwards |
| `urgent` | immediately, when a task slot is free (one extra slot is reserved for urgent work); its agents go first in the spawn queue |
| `normal` | when fewer than `max_concurrent_tasks` tasks are running |
| `when_free` | only when no task is running **and** all sessions have been idle for `idle_grace_secs` |
| `on_demand` | only when you run `moo task run <id>` |

### 8.2 Sorting and editing

```bash
moo task ls
#   ID STATUS     LABEL       RANK  TITLE
#    2 running    urgent         2  Prod: 500s on /checkout since 14:00
#    1 queued     when_free      1  Upgrade pydantic to v3 and fix breakages
#    4 queued     when_free      4  Add pagination to /orders
#    3 queued     on_demand      3  Write release notes for 2.4

moo task order 4 1                  # 4 first, then 1, then everything else as before
moo task edit 1 --label urgent      # promote
moo task add "Prod is down" --label right_now   # pauses less urgent work
moo spawn gen-fast -p "Review auth.py" --system-prompt "You are a security reviewer."   # a generic agent
moo task edit 4 --body "Cursor-based, default page size 50"
moo task run 3                      # start an on_demand task now
moo task cancel 1                   # stops its coordinator and every agent it spawned
moo task rm 3                       # delete (not while running)
```

### 8.3 What happens when a task starts

1. A coordinator session starts (`task #2 coordinator`) in a shared workspace `task-2`, with the task as its prompt.
2. It plans with todos and starts agents with `cluster_spawn_agent` (e.g. coder with `share_workspace: true`).
3. Whenever one of its agents finishes a turn, the coordinator gets a message: *"Session s-… (coder, "fix checkout") finished a turn. Todos 3/3 completed. Output: …"*.
4. It reviews, delegates more or replies, and finally calls `cluster_task_complete("summary")`. The task becomes `done`, the summary is stored as its result, and all its sessions stop.
5. If it stops without finishing, it is nudged up to `tasks.max_nudges` times, then the task is marked `failed`.

Watch it live:

```bash
moo task show 2 --json | jq '{status, coordinator_session, notes, result}'
moo events -f | grep -E 'task\.|turn_ended|spawn'
moo sessions          # children show the coordinator as their parent (in --json)
```

Steer a running task the same way as any session:

```bash
moo send <coordinator-session> "Don't touch the payments module; open a TODO instead."
```

### 8.4 Tuning the scheduler (`supervisor.yaml`)

```yaml
tasks:
  coordinator_agent: coordinator
  coordinator_node: hub          # optional pin
  max_concurrent_tasks: 2
  idle_grace_secs: 120           # how quiet the cluster must be before when_free work starts
  max_nudges: 2
  enabled: true                  # false = nothing starts unless you `moo task run`
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
moo spawn coder    --shared payments -p "Implement refunds in payments/refund.py"
moo spawn reviewer-fast --shared payments -p "Wait for a message, then review payments/refund.py"

# Seed a shared workspace with an existing repo, then point agents at it
git clone https://example.org/me/payments ~/.moocher/data/workspaces/shared-payments
# (rootful podman: sudo chown -R 1000:1000 that directory)

# Grab results from the host side; no podman cp needed
ls ~/.moocher/data/workspaces/shared-task-2/
```

Shared workspaces have no locking. Tell agents (or the coordinator) who owns which files.

**Across machines.** A shared workspace can be used by sessions on different nodes; each node has its own copy and the hub moves changes between them through the artifact store:
- before a session starts on a node whose copy may be behind (it shows in `moo show`: `placed: … workspace demo: synced from hub`);
- after every turn, towards the other machines that have live sessions in it;
- when an agent calls `cluster_sync_workspace`.

Each sync is a file-by-file three-way merge against the last state both sides saw. A file changed on only one side takes that side's version (added, updated or deleted). A file changed on **both** sides keeps the local version, and the incoming one is written next to it as `<file>.theirs`; the agents in that copy are told which files to reconcile. Workflow runs use this too: a step can run on another machine than the previous one (`node:` per step, or placement), and the run's workspace follows it.

**Handing over files without sharing a workspace:** artifacts (§20).

### 9.1 Git repositories in workspaces

Agents commit as one person, set once on the hub:

```yaml
# supervisor.yaml
git: { name: Robel, email: robel@example.org }
```

Every agent commit gets that author and committer. Which agent made it goes into a git note instead of the message:

```bash
git log --notes=moo -1
# commit 9f2c…
# Author: Robel <robel@example.org>
#     Add the refunds endpoint
#
# Notes (moo):
#     moocher-agent: coder
#     moocher-session: s-3f9a1c2b7d
#     moocher-model: gemini/gemini-flash-latest
#     moocher-node: hub
#     moocher-task: 7
#     moocher-parent: s-77ab…
```

Notes follow amends and rebases (`notes.rewriteRef` is set in the agents' environment). To publish them: `git push origin refs/notes/moo`. A repository's own hooks keep working; the cluster's hook runs them first.

**Across machines,** `.git` is never merged file by file (that corrupts it). A sync carries each repository's history as a git bundle (repositories up to three directories deep; `node_modules`, `target`, `.venv` are skipped) and the receiving copy fetches it as `refs/remotes/moo-<node>/…`, then moves its branches forward where that is a fast-forward: the checked-out branch with its index, so `git status` stays clean. Branches committed on both machines since the last sync stay apart, and the agents there are told to merge `moo-<node>/<branch>`. Checkpoints and `resume_elsewhere` keep the history the same way.

**Fork, work on a branch, hand back.** A coordinator calls `cluster_spawn_agent(…, fork_workspace: true)`: the child starts from a copy of its workspace, history included, on branch `agent/<agent>/<session>`. A child template that ends with

```yaml
outputs:
  - { git: true, to: parent_workspace }            # merge: ff (default) | merge | none
```

brings that branch back when it stops: the parent's repository gets `refs/remotes/<session>/agent/<agent>/<session>`, its checked-out branch is fast-forwarded if it can be, and the parent is told (`[cluster] Session s-… handed back its work: … merged into the checked-out branch (fast-forward)`). If it cannot fast-forward (the parent committed meanwhile), the parent merges it itself.

Git runs inside a throwaway container of the agent image for all of this (no network, the agent's user), never on the host: a repository's config and hooks belong to the agent.

---

## 10. Budgets, limits and egress routes

Per agent:

```yaml
limits:
  max_sessions: 2          # concurrent sessions of this agent per node
  budget_usd: 1.00         # per session; keyproxy then answers "[moocher] session budget $1.00 exhausted"
  max_tokens: 500000       # per session
  wall_clock_secs: 1800    # aborts the session's work after 30 min
container:
  memory: 1g               # podman --memory
  cpus: 1.0
  pids: 512
  egress: none             # none = no network at all (recommended); allowlist = HTTP(S) to listed hosts (§29); full = normal networking
```

Budgets and token caps count what the provider reported, not only what OpenCode says (§28).

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
moo quotas kilo-free          # per-IP buckets: used/limit, left, RESETS IN 3h 24m, source (header/429/config/override)
moo ips                       # which nodes and sessions share each IP
moo quota set kilo-free --scope-id 203.0.113.7 --resets-at 03:15 --remaining 0   # you know better than the counters
moo route set vps-fi free-coder --prefer via-relay-de --failover direct          # live, kept in that node's overrides.yaml
```

When every route is used up, the session is `parked`, and `moo status` shows the countdown. It resumes by itself after the reset. For BYOK accounts use `scope: account` (keys labelled with the same `account`) or `scope: key`. Their limits and resets are learned from the provider's rate-limit headers.

---

## 11. Monitoring: status, events, metrics

```bash
moo status                       # nodes, sessions (state, todos, tokens, cost), tasks, totals
moo tui                          # dashboard in the terminal (or the web dashboard, §23)
moo events --limit 100           # recent event log
moo events -f                    # follow live
moo events -f --session s-81c2…  # one session: tool calls, state changes, turn ends
moo events --json | jq -c 'select(.kind=="run.verdict") | .data'
```

Event kinds you'll see:
- **Sessions:** `session.state`, `session.turn_ended`, `tool.called`, `tool.failed`, `permission.asked`
- **Workflow runs:** `run.started`, `run.step`, `run.nudge`, `run.verdict`, `run.finished`
- **Tasks:** `task.started`, `task.finished`
- **Nodes:** `node.attached`, `node.detached`, `node.agents`
- **Providers:** `provider.context_overflow`, `provider.credits_exhausted`, `provider.auth_failed`
- **Spawn queue:** `spawn.queued`, `spawn.dequeued`, `spawn.expired`, `spawn.cancelled`

### 11.0 Provider units

If a provider meters in neurons, pollen or credits, declare the unit (`quota.units`, [CONFIG.md](./CONFIG.md)) and watch what your tokens cost on each model:

```bash
moo quotas my-provider
# my-provider key key:7a72… unit:neurons 62/100 38 12h 34m measured hub
#     big: 20093.8 neurons/Mtok, 1 neurons buys ~50 tokens (measured)
#     small: 332.9 neurons/Mtok, 1 neurons buys ~3003 tokens (measured)

# The provider reports nothing? Read its dashboard and tell the cluster:
moo quota set my-provider --unit neurons --used 4210
```

The dashboard's Providers & quotas page has the same numbers, with a **calibrate** button on each pool.

### 11.1 What the cluster has learned about providers

keyproxy watches every call it makes and the nodes report it to the hub once a minute, as hourly rows per provider model, key or IP, and route: calls, failures by class (`rate_limited`, `credits`, `context_overflow`, `auth`, `client`, `server`, `unreachable`), time to the first byte, output tokens per second, tokens and cost.

```bash
moo providers                   # last 24 h, one line per model
# MODEL                                      CALLS   ERR%  1ST BYTE       p95    TOK/S       COST LEARNED
# groq/llama-3.3-70b-versatile                 214   1.4%    ≤0.4s     ≤1.6s      310    $0.0000             rate_limited:3
# gemini/gemini-flash-latest                    88   0.0%    ≤0.8s     ≤3.2s      140    $0.0123
# cohere/command-a-03-2025                      12  16.7%    ≤1.6s     ≤6.4s       45    $0.0000 window 128000    context_overflow:2
# openrouter/qwen/qwen3-coder                   40   2.5%    ≤0.8s     ≤1.6s       90    $0.0310 out of credits 2h ago credits:1
moo providers groq --hours 168  # one provider, a week
moo providers --events          # context overflows, credits, refused keys, newest first
```

What it learns and uses:
- **Context windows.** When a provider refuses a request as too long and names its window ("maximum context length is 32768 tokens"), that window is remembered. New sessions of that model are configured with it if it is smaller than the template's `context:`, so OpenCode compacts before the provider refuses. If the template claims more, you get a notification.
- **Credits.** A `402`, or an error saying the account is out of credits (`insufficient_quota`, "insufficient credits", …), is recorded with the time, notifies you, and holds that key's allowance back for an hour (the quota shows `source: credits`); a plain rate limit is not mistaken for it.
- **Speed and reliability** go into `cluster_list_agents` ("first byte ≤0.4 s (p50), 310 tok/s, 1% errors over 214 calls") so agents can pick, and into the dashboard.

`opencode-free` traffic skips keyproxy, so it has no observations.

### 11.2 Spawning when there is no room

A coordinator calling `cluster_spawn_agent` gets an error when no node can take the agent now. With `if_busy: "wait"` (and optionally `wait_max_s`, default one hour) it is queued on the hub instead: it starts as soon as a slot frees up or the allowance resets, and the coordinator gets a message with the new session id (`[cluster] solo (queued as q-…) has started: session s-…`). `cluster_stop_session("q-…")` cancels it. The queue: `curl …/api/v1/spawn-queue`. The agents are told in `cluster_list_agents` whether waiting is worth it, and can ask you with `cluster_ask` instead.

### Prometheus

```yaml
# prometheus.yml
scrape_configs:
  - job_name: moocher
    authorization: { credentials_file: /etc/prometheus/moo-token }   # a read-scope token (§13)
    static_configs: [ { targets: ["127.0.0.1:7700"] } ]
```

Metrics: `moo_nodes_connected`, `moo_sessions{state}`, `moo_tokens_total{agent,kind}`, `moo_cost_usd_total{agent}`, `moo_tasks{status}`.

Example queries:
- `sum(moo_cost_usd_total)`
- `sum by (agent) (rate(moo_tokens_total[1h]))`

---

## 12. REST API cookbook (curl)

All endpoints are under `/api/v1` and need `Authorization: Bearer <token>`. Mutating calls need `operate`; tokens and the audit log need `admin`.

```bash
API=http://127.0.0.1:7700/api/v1
H="Authorization: Bearer $(cat ~/.moocher/cli-token)"

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
curl -sN "$API/stream?token=$(cat ~/.moocher/cli-token)"   # for EventSource clients

# Metrics, health
curl -s -H "$H" http://127.0.0.1:7700/metrics
curl -s http://127.0.0.1:7700/healthz                              # no auth
```

Errors come back as `{"error": "…"}` with 400, 401, 403 or 404.

---

## 13. Access tokens and the audit log

```bash
moo token create grafana --scope read       # dashboards, Prometheus
moo token create ci-bot --scope operate     # scripts that add tasks / spawn sessions
moo token create backup-admin --scope admin
moo token ls
moo token revoke ci-bot

# Offline (supervisor stopped), e.g. if you lost the admin token:
moocher-hub token create rescue --scope admin
```

Each token is shown once and stored only as a hash.

Every control action is in a hash-chained audit log. The actor is `operator:<token>`, `session:<id>` for agent tool calls, or `system`.

```bash
moo audit --limit 20
# 22:00:13 operator:admin   session.spawn   s-e3a64da210  ok
# 22:00:48 system           supervisor.stop -             ok
# 22:02:25 operator:admin   session.spawn   -             FAILED
moo audit --verify            # {"ok": true, "entries": 66}
moocher-hub audit-verify   # same, offline
```

---

## 14. Scripting recipes

**Wait for a workflow run and fail CI if it didn't pass**

```bash
RUN=$(moo workflow run build-review "$REQUEST" --json | jq -r .id)
while :; do
  ST=$(moo workflow show "$RUN" --json | jq -r .status)
  [ "$ST" != running ] && break; sleep 10
done
echo "run $RUN: $ST"; [ "$ST" = passed ]
```

**Nightly maintenance tasks (cron)**

```cron
0 1 * * *  MOO_TOKEN=moo_… moo task add "Update dependencies, run tests, summarise breakages" --label when_free
30 1 * * 1 MOO_TOKEN=moo_… moo task add "Weekly: triage TODO comments into issues.md" --label when_free -t weekly
```

**Turn a TODO file into tasks**

```bash
grep -E '^- \[ \]' TODO.md | sed 's/^- \[ \] //' | while read -r line; do
  moo task add "$line" --label when_free
done
```

**Spend report per agent (today)**

```bash
moo sessions --all --json | jq -r --argjson since $(( $(date -d today +%s) * 1000 )) '
  map(select(.started_at >= $since)) | group_by(.agent)[]
  | "\(.[0].agent)\t\(map(.tokens.input + .tokens.output) | add) tok\t$\(map(.tokens.cost_usd) | add)"'
```

**Stop every idle session older than an hour**

```bash
moo sessions --json | jq -r --argjson cut $(( ($(date +%s) - 3600) * 1000 )) \
  '.[] | select(.state=="idle" and .updated_at < $cut) | .id' | xargs -rn1 moo stop
```

**Alert when something gets blocked**

```bash
moo events -f --json | jq --unbuffered -c 'select(.kind=="permission.asked")' |
while read -r ev; do notify-send "agent needs permission" "$ev"; done
```

---

## 15. Running as systemd services

Ready-made units are in `deploy/`; `tests/deploy-check.sh` runs `systemd-analyze verify` and `security` on them and checks the Quadlet.

| File | What |
|---|---|
| `deploy/systemd/moocher-hub.service` | The hub, as user `moo`, config and state in `/var/lib/moo/hub`, link socket in `/run/moo-hub`. Fully sandboxed (`systemd-analyze security`: 1.6, "OK"); only `CAP_NET_BIND_SERVICE` for ports 80/443. |
| `deploy/systemd/moocher-node.service` | A node with rootful Podman, config in `/etc/moo/node`. Lighter sandbox (Podman needs mounts and namespaces; the agents' sandbox is their container). `KillMode=process`: agent containers survive a restart of the daemon and are re-adopted. |
| `deploy/systemd/moocher-node.user.service` | A node with rootless Podman as a user service (`loginctl enable-linger`). |
| `deploy/quadlet/moocher-hub.container` + `images/supervisor/Containerfile` | The hub as a Podman Quadlet: read-only root, no capabilities, uid 10001. |

```bash
sudo useradd --system --home-dir /var/lib/moo --shell /usr/sbin/nologin moo
sudo install -m 0644 deploy/systemd/moocher-hub.service deploy/systemd/moocher-node.service /etc/systemd/system/
# supervisor.yaml in /var/lib/moo/hub:  link_socket: /run/moo-hub/link.sock
# node.yaml in /etc/moo/node:           supervisor: unix:///run/moo-hub/link.sock
sudo systemctl daemon-reload && sudo systemctl enable --now moocher-hub moocher-node
```

Provider keys as encrypted credentials, referenced in agents as `systemd-creds:<name>`:

```bash
sudo systemd-creds encrypt --name=openrouter - /etc/moo/creds/openrouter.cred   # paste key, Ctrl-D
sudo systemctl edit moocher-node     # add: [Service] LoadCredentialEncrypted=openrouter:/etc/moo/creds/openrouter.cred
journalctl -u moocher-node -o cat | jq -r '.fields.message'      # logs are JSON
```

`MOO_LOG=debug` (or e.g. `MOO_LOG=info,moocher_node::manager=debug`) raises verbosity.

---

## 16. Testing offline with the mock LLM

`examples/mock-llm/mock_llm.py` is an OpenAI-compatible server that replays scripts. The model name selects the script, so the `builder` model uses `scripts/builder.json`. It's great for testing workflows, permissions and the task bucket without spending tokens.

```bash
python3 examples/mock-llm/mock_llm.py --port 8099 --require-key test-key &
export MOCK_API_KEY=test-key     # for moocher-node; proves keyproxy injects the real key
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
| Session shows `lost` or `dead` | `moo halts`, then `moo recover <halt> restart|resume_elsewhere|handoff|abandon`. | The cheapest recovery may already have run by itself (§21). |
| Task shows `halted` | `moo halts` shows the options with their costs; `moo recover <h-id> <option>` resumes everything. | Halted sessions keep their containers until then. |
| Stuck in `starting` > 2 min | `podman logs moo-<session>`; OpenCode log under `data/instances/<session>/home/.local/share/opencode/log/` | — |
| `[moocher] …` error in a session | It came from keyproxy: budget, token cap, model allowlist or missing secret. Check `moocher-node check`. | — |

Where data lives (default `~/.moocher`):

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
podman ps -a --filter label=moo.session            # every agent container
moo sessions --json | jq -r '.[] | select(.state=="idle") | .id' | xargs -rn1 moo stop
sqlite3 ~/.moocher/supervisor/supervisor.db "DELETE FROM events WHERE ts < strftime('%s','now','-30 days')*1000"
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
moo templates                                   # where each template can run, and why not elsewhere
moo spawn coder --append "Focus on the parser" --budget 2 --route via-relay-de
moo show <session> --preamble                   # the rendered instructions it started with
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
moo questions                          # open ones: importance, then how many wait on it, then age
# q-3f2a…  vital  open  ask  blocked 2  1m  on worker-a task #3
#     May I delete the old data directory?
#     chain: builder s-81c2… → s-77ab… → operator   held by operator
#     note (s-77ab…): the coordinator does not know whether it is still used
#     raised to vital: mentions "delete"
moo answer q-3f2a… "Yes, it was migrated last week"
moo answer q-3f2a… --choice 2          # pick an option
moo answer q-3f2a… --decide            # "let the agent decide" (logged as an assumption)
moo answer q-3f2a… --reject

moo questions mode                     # cluster mode and per-task modes
moo questions mode vital_only          # switch live
moo questions mode autonomous --task 7 # or for one task (also: moo task add … --question-mode …)
moo assumptions [--task 7]             # decisions made without you, for review
```

**Budgets:** when keyproxy refuses a call because a session's `budget_usd` is used up, you get a `vital` operator-only question: `moo answer q-… "raise 2.50"` raises it live, `stop` stops the session (its outputs are harvested).

**Upward reports:** `cluster_report_up(message)` sends a status report to the parent, or for a top-level agent, to the task notes and the notification centre (`moo notifications`).

Questions are also in `GET /api/v1/questions` (and on the SSE stream), answered with `POST /api/v1/questions/{id}/answer`.

---

## 20. Artifacts

Content-addressed files and directory trees, stored on the hub (`art://b3/<hash>`). Identical content has the same reference. Metadata records who made it, on which node, for which task, and **lineage**: the artifacts it was made from.

```bash
moo artifact put report.pdf --tag release
moo artifact put ./build-output --name build-42      # a directory becomes a tree (tar.zst)
moo artifact ls [text] [--task 3]
moo artifact show art://b3/9f2c…                      # metadata, parents ("from") and children ("used by")
moo artifact get art://b3/9f2c… -o out/               # a tree is unpacked into the directory
```

**Agents** use:
- `cluster_artifact_put(path)` returns a reference, which they can attach to `cluster_send_message(…, artifacts=[ref])`;
- `cluster_artifact_get(ref, path?)` puts it into their workspace;
- `cluster_artifact_list` and `cluster_artifact_info`.

Whatever an agent publishes lists the artifacts it fetched before as parents, so `moo artifact show` traces where a result came from.

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
moo halts
# h-2e1c…  open  task:3  s-81c2… died (exit:137)  spent $0.4120  2m
#     was doing: write tests
#     halted: s-77ab…, s-90fe…
#     * restart           ~$0.0310   same node worker-a, history intact; re-reads ~62000 tokens of context
#     * resume_elsewhere  ~$0.0310   on hub (best placement); workspace: snapshot taken now from the dead instance's volume
#     * handoff           ~$0.2060   parent s-77ab… spawns a replacement with its own summary
#     * abandon           ~$0.0000   give up: the step/task fails with the death reason
moo recover h-2e1c… restart                     # one call resumes the whole tree
moo recover h-2e1c… resume_elsewhere --node hub
moo recover s-81c2… restart                     # a dead session without a halt
```

**Checkpoints:** every `checkpoint_every_turns` (default 3) finished turns, a node sends a snapshot of the workspace and the transcript to the hub. If a machine is lost, the replacement starts from there. The harvest record says that anything written after the checkpoint is unrecoverable. If the lost machine comes back later, its old instance of a replaced session is stopped.

---

## 22. Outputs, harvesting and clean-up

Templates name the files worth keeping (`outputs:` in CONFIG.md). They are collected when an instance is stopped, dies, is halted, reaches its wall clock, or its task ends:

```yaml
outputs:
  - { path: report.md, to: parent_workspace, dest: from-builder/report.md }
  - { path: "dist/*.whl", to: artifact, name: wheels }
  - { git: true, to: parent_workspace }       # its commits, into the parent's repository (§9.1)
transcript: true
retain: on_failure
```

```bash
moo harvests --session s-81c2…
# 14:02:11 s-81c2… on worker-a (stop)
#     report.md      art://b3/77e1…   812B → parent_workspace isolated:s-77ab… on hub: from-builder/report.md
#     dist/*.whl     art://b3/1c4d…  2048B → artifact
#     missing.txt    (matched nothing)
#     transcript art://b3/a09b…
```

The lifecycle is `running → stopping → harvesting → stopped → reaped`. After the harvest, the capability token stops working (the instance's gateway is gone). `retain:` decides when the OpenCode home and isolated workspace are removed (default 24 h); `moo stop --rm-workspace` removes them at once.

---

## 23. The web dashboard and login

The supervisor serves the dashboard itself (no separate web server): open `http://127.0.0.1:7700` on the hub, or your domain once HTTPS is set up (§24).

```bash
moocher-hub user set admin                 # asks for a password (≥ 10 characters)
moocher-hub user set alice --scope operate # read | operate | admin, as for API tokens
moocher-hub user totp admin                # authenticator-app codes (prints the secret / otpauth:// URI)
moocher-hub user list
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
    domains: [moo.example.org]
    email: you@example.org
    http01_listen: 0.0.0.0:80      # challenges; everything else redirects to https
attach_listen: 0.0.0.0:7444        # Open in OpenCode (§25), same certificate
```

- **First start:** the hub orders the certificate from Let's Encrypt. To try it first, set `directory: https://acme-staging-v02.api.letsencrypt.org/directory`.
- **Storage:** the certificate is kept in `<data>/acme/` (keys 0600).
- **Renewal:** it renews 30 days before expiry and is swapped in without a restart.
- **Ports below 1024:** binding them needs `CAP_NET_BIND_SERVICE` (`AmbientCapabilities=CAP_NET_BIND_SERVICE` in the unit) or root.

**Your own certificate:** `tls: { cert_file: /etc/moo/fullchain.pem, key_file: /etc/moo/key.pem }`. Replacing the files is picked up within a minute. It must be a server certificate: a self-signed certificate that is its own CA is rejected by rustls-based clients. Use a small private CA or mkcert, and give the CLI `MOO_CA_CERT=/path/ca.pem`.

**Behind Caddy or nginx:** keep `api_listen` on loopback, set `trusted_proxies: [127.0.0.1]` (so lockouts see the real client IP and cookies get `Secure`), and proxy both the dashboard port and the attach port.

`moo` talks to HTTPS with `MOO_URL=https://moo.example.org`.

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
moo attach s-81c2…                  # runs `opencode attach` against a local proxy that adds the cookie
moo attach s-81c2… --no-launch      # just prints the local address (e.g. for a second terminal)
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
- **Webhook:** set `webhook: { url: …, secret: "file:/etc/moo/hook" }`. It POSTs JSON `{title, body, kind, severity, target, id, ts}`, signed with `X-Moo-Signature: sha256=…`.
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
moo tui
```

- **Views:** instances, questions, halts with tasks, and nodes. Switch with `1`–`4` or Tab.
- **Keys:** ↑/↓ select, Enter shows details, `a` answers a question, `m` messages an instance, `x` stops it, `r` recovers a halt (cheapest option), `q` quits.
- **Refresh:** every 2 s.
- **Remote use:** it works over SSH with the same API token as `moo`.

---

## 28. Usage metering

OpenCode reports its tokens from inside the container, where the agent also has a shell and the capability token for keyproxy. So keyproxy keeps its own count. It reads the usage the provider puts in every response: in JSON bodies and in streams, without holding the stream back. It understands four formats:
- OpenAI-compatible chat;
- the OpenAI Responses API;
- Anthropic's `message_start` / `message_delta`;
- Gemini's `usageMetadata`.

For OpenAI-compatible streams, keyproxy adds `stream_options.include_usage` when the request lacks it.

- `moo show <id>` prints the metered count when it differs. The dashboard shows the larger of the two counts as the session's tokens and cost, and so do `moo sessions`, stats and recovery cost estimates.
- `budget_usd` and `max_tokens` are enforced on the larger count. A call made around OpenCode with `curl` and the token still runs into the budget.
- When the meter is ahead of OpenCode by more than 25% and 2000 tokens, the session raises `usage.mismatch`, a warning notification, once.

## 29. Network allowlists (`egress: allowlist`)

```yaml
container:
  egress: allowlist
  allow_hosts: [pypi.org, "*.pythonhosted.org", github.com, "*.githubusercontent.com"]
```

The container keeps `--network=none`. Inside it, `HTTP_PROXY`/`HTTPS_PROXY` (and lower-case) point at `127.0.0.1:3128`, which the bridge forwards to a proxy run by the node for this instance alone. pip, npm, git, curl and Bun pick it up. The proxy:
- **Accepts two request forms**: `CONNECT host:port`, a byte tunnel that keeps TLS end to end, and plain `http://` requests.
- **Checks host and port** against the list on every connection, so edits apply at once. An entry without a port means 80 and 443; `*.domain` means subdomains only.
- **Refuses internal addresses**: a listed name that resolves to a loopback, private, link-local or CGNAT address is refused, so an allowed name cannot be pointed at the node or your LAN. List the address itself (`10.0.0.5:3128`) to allow one.
- **Records refusals** as `egress.denied` events, with the reason, at most once per host per 10 minutes.

## 30. Housekeeping: event export and artifact expiry

Every hour the hub:
- **Exports events** by UTC day to `<data_dir>/exports/events-YYYYMMDD.jsonl.zst`, one JSON event per line. The export is incremental, so every event is written exactly once, including late ones.
- **Prunes events** older than `events.retain` (30 days) from the database, but only once they are exported.
- **Expires artifacts** older than `artifacts.retain` (off by default). Artifacts tagged `keep` are spared, and so is anything still referenced: live sessions, open tasks and runs, open halts, checkpoints, shared workspaces, and `art://b3/…` in config files such as an agent's `workspace.seed`.

```bash
moo housekeeping --dry-run --ahead 1d       # what tomorrow's run would do, changing nothing
moo housekeeping                             # run now (admin; audited)
zstdcat ~/.moocher/supervisor/exports/events-20261024.jsonl.zst | jq -c 'select(.kind=="session.dead")'
moocher-hub events-cat …/events-20261024.jsonl.zst     # the same without zstd installed
```

## 31. Hard disk limits

`container.disk` is soft by default (§10). For a limit the kernel enforces, set `disk_enforce: hard` in `node.yaml`:
- **Requirements**: the node's data dir on XFS mounted with `prjquota`, or on btrfs, and the node running as root.
- **What it limits**: the instance's OpenCode home and its isolated workspace share one XFS project or btrfs qgroup, so a write past the limit fails with "Disk quota exceeded".
- **Startup check**: the node checks all of this at start and refuses to run, with the reason, where it cannot work.
- **Shared workspaces** belong to several sessions and stay soft.

## 32. What's coming

| Feature | Phase |
|---|---|
| ~~Remote nodes over mTLS, join tokens, `via_node` routes, quotas and failover, templates~~ | 2 ✔ (web UI for these: 4) |
| ~~Artifact store, cross-machine shared workspaces, lineage, questions and escalation, dead-agent recovery and halts, harvesting~~ | 3 ✔ (UI views and push notifications for these: 4) |
| ~~Web UI (SolidJS, mobile-first), TUI, password login, built-in TLS, Open in OpenCode, notifications~~ | 4 ✔ |
| ~~Load and chaos tests, Quadlet and systemd units, event export, artifact expiry, keyproxy usage metering, egress allowlists, hard disk quotas~~ | 5 ✔ |
| Pre-warmed containers (instance boot is ~4 s; see PHASE5.md) | later |

## The manager

The round button at the bottom right of the dashboard, or `moo ask "…"`, puts a question to the manager: what is running where, why something is paused, what a provider has left. Ask it to go somewhere ("take me to the quotas") and the dashboard goes there.

Turn it on by naming an agent whose model it should use, in `supervisor.yaml`:

```yaml
manager: { agent: gen-fast }
```

It runs no instance and takes no slot, and calls go through a node's keyproxy, so routes and allowances apply. It keeps nothing: each question reads the cluster again, the conversation stays in the browser tab (the last six messages go along as text so follow-ups make sense), and only the fact of a question and its cost is audited. Its tools only read, and it can send the browser only to dashboard pages. Asking needs the `operate` scope, since it costs money.

## The work map

The **Work map** page draws each task (or workflow run) as a graph of who started whom. Each card shows the agent, state, node (the colored edge and chip), model, start time, cost, why it is paused and how many messages are waiting for it. Dashed lines join a replacement to the session it replaced. `/work#task-12` opens at one task.

## Paused sessions and their messages

A session paused for `right_now` work keeps its conversation, todos and workspace. The turn it was in is cut off: an unfinished model reply is discarded and a running command stops, and it is told so when it resumes.

Messages for a session that is not running (paused, restarting, dead or on an unreachable node) wait at the hub. When it runs again it gets them in one message, oldest first, marked as past events with how old each one is. A replacement inherits them. If the session ends for good they are dropped, and a `message.dropped` event records how many. Sessions show the number waiting, and `message.held` and `message.delivered` events record the rest.

## Generic agents and reserved models

A generic agent is only a model. Whoever starts it says what it should be:

```yaml
# agents/gen-fast.yaml
name: gen-fast
kind: generic
provider: { id: anthropic, model: some-model, key: { secret: anthropic } }
```

```bash
moo spawn gen-fast -p "Review this diff" --system-prompt "You are a careful reviewer."
```

A coordinator does the same through `cluster_spawn_agent`'s `system_prompt`. The prompt is required for a generic agent and refused for any other. Tools and limits still come from the config, so a spawner cannot widen them.

To keep a costly model for urgent work, add `min_urgency: right_now` (or another level) to its agent. An agent below that level is refused it; you can always start it by hand.

## Pausing a task

```bash
moo task pause 12        # also on the Tasks page
moo task pause 12 --now  # cut off what its agents are doing
moo task resume 12
```

A pause stops every agent of the task and keeps its conversation, todos, workspace and container. The task shows as `pausing` until all have stopped, then `paused`, and its agents give up their slots.

- **Default:** each agent stops at its next model call, so the reply and any command under way finish first. Idle agents, and those waiting for an answer or for other agents, stop at once. After `tasks.pause_grace_secs` (default 2 minutes), any still going are cut off.
- **`--now`:** stops them at once, the coordinator first. The agents are told on resume that a command may not have finished.
- **While paused:** messages wait at the hub and arrive as one digest on resume. Questions stay open and do not escalate. Spawns the task had queued wait. The task does not count toward `max_concurrent_tasks` and holds no other work back, even at `right_now`.
- **Resume** follows the rules for starting work: unless the task is `right_now`, it waits for `right_now` work and the task cap. An agent also preempted by `right_now` work comes back when that work is done.
- **A queued task** can be paused too, which keeps it from starting; resume puts it back in the queue.
- **Shared workspaces:** other tasks' agents can still change files during a pause. Prompt caches may expire, so the first call after a long pause can cost more.
- **Workflows:** a step that finishes while the task is pausing still starts the next one, which is paused at its first model call.

## Several API keys

List them on the provider, on each node that has them (`providers.yaml`), and leave `key` off the agents:

```yaml
providers:
  groq:
    keys:
      - { ref: "env:GROQ_KEY_1" }
      - { ref: "env:GROQ_KEY_2", lend: true }
```

Sessions start on the least used key and move to the next when one runs out or is refused, with no restart. `moo pool` (or **Providers & quotas**) shows each key by a short hash, its state, who uses what and anything kept for urgent work. `key.switched` events record each move.

To spread sessions across IPs as well, give the agent `route: { prefer: direct, failover: [via-vps], balance: true }`.

When `right_now` work hits a limit, less urgent sessions keep off that key or IP until it is done. They move to another, or wait at their next call. Nothing is lost.

A node out of keys can borrow: `lend: true` on the key, `borrow: true` on the provider of the node that needs it. The key travels encrypted and stays in memory for an hour. Lending means the key leaves its machine: any approved worker can ask for it, and a compromised hub could obtain it. Only mark keys you accept that for.
