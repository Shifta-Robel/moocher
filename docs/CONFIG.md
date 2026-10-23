# Configuration reference

All files are YAML in one directory per machine (default `~/.opencode-cluster`, override with `--config-dir` or `OCL_CONFIG_DIR`). The directory is meant to be version-controlled: it contains **secret references, never secrets**.

`opencode-node schema --out schemas/` writes JSON Schemas for every file, for editor validation. `opencode-node check` validates the directory and resolves every secret reference without printing it.

```text
node.yaml          every machine running opencode-node
supervisor.yaml    hub only
providers.yaml     optional; merged over built-in providers
routes.yaml        optional; egress routes
agents/*.yaml      one agent profile per file  ← the unit of organization
prompts/*.md       role prompts referenced by agents
workflows/*.yaml   hub only
templates/*.yaml   hub only: agent templates pushed to every node
preambles/cluster.md  hub only: first part of every agent's instructions
overrides.yaml     written by the node for live changes from the CLI/UI (not for version control)
pki/               node key + certificate + hub CA (written by `opencode-node join`)
```

Complete examples for a hub, a remote worker and a relay are in [`examples/remote/`](../examples/remote/).

**Hot reload:** the node watches `node.yaml`, `providers.yaml`, `routes.yaml`, `overrides.yaml`, `agents/`, `prompts/` and `workflows/`. The hub re-reads `templates/` and `preambles/` every 10 s and pushes changes to the nodes.
- **Apply immediately:** provider, route, key and limit changes (keyproxy reads them per request).
- **Apply to new sessions:** role, tools and container changes.
- **Errors:** a file that fails to parse is reported and ignored, and the last good config stays active.

## node.yaml

| Key | Default | Meaning |
|---|---|---|
| `name` | — | Node name, `[a-z0-9-]{1,48}`, unique in the cluster. Must match the name the node enrolled as. |
| `supervisor` | — | `unix:///run/ocl/supervisor.sock` (same machine) or `https://hub.example.org:7443` (remote, mTLS; written by `opencode-node join`). |
| `role` | `worker` | `worker` runs agents. `relay` runs none and only lends its IP to other nodes (`via_node` routes); it needs no Podman. |
| `pki_dir` | `<config>/pki` | Node key (0600), certificate and hub CA. |
| `max_sessions` | `6` | Concurrent live sessions on this node (~420 MB RAM each). |
| `relay` | `false` | A *worker* that also relays traffic for other nodes. |
| `relay_ports` | `[443]` | Destination ports this node relays to. |
| `relay_allow_hosts` | `[]` (any) | Destination hosts this node relays to: `api.groq.com`, `*.example.com`. |
| `egress_bind` | — | Local address LLM traffic leaves from, on hosts with several IPs (direct routes and relayed traffic). |
| `ip_echo` | `["https://api.ipify.org"]` | URLs that return the caller's IP, used to learn each route's egress IP. `[]` disables probing; remote nodes still learn their direct IP from the hub. |
| `secrets` | `{}` | Secret aliases for hub templates: `openrouter: { ref: "systemd-creds:openrouter", account: personal }`. |
| `hub_templates` | `true` | `false`: ignore the hub's template library, run only `agents/`. |
| `default_route` | direct | Route for agents that don't set `route`. |
| `labels` | `{}` | Free-form, shown in the UI. |
| `runtime.kind` | `podman` | `podman`, or `process` (dev/CI only: no isolation). |
| `runtime.podman` | `podman` | Podman binary. |
| `runtime.image` | `localhost/ocl-agent:1.19.4` | Default agent image. |
| `runtime.run_dir` | `$XDG_RUNTIME_DIR/ocl` or `/run/ocl` | Per-session socket dirs (0700). |
| `runtime.data_dir` | `<config>/data` | Workspaces, OpenCode homes, `node.db` (outbox). |
| `runtime.opencode_bin` | `opencode` | `process` runtime only. |

## supervisor.yaml

| Key | Default | Meaning |
|---|---|---|
| `api_listen` | `127.0.0.1:7700` | Operator API. Keep it on loopback, or put a TLS proxy in front until Phase 4. |
| `link_socket` | `$XDG_RUNTIME_DIR/ocl/supervisor.sock` or `/run/ocl/supervisor.sock` | Unix socket (0600) for same-machine nodes. |
| `link_listen` | off | TCP address for remote nodes (mTLS), e.g. `0.0.0.0:7443`. |
| `public_names` | hostname, `localhost`, `127.0.0.1` | Names/IPs remote nodes use for the hub; they go into the link certificate (reissued when they change). The first one forms the join command. |
| `enrollment.require_approval` | `true` | Enrolled nodes stay `pending` until `ocl node approve <name>`. |
| `enrollment.allowed_sources` | `[]` | IPs/CIDRs allowed to enroll and connect. Seeds the list on first start; afterwards `ocl allow add|rm` (audited) is authoritative. |
| `enrollment.enforce_for` | `relay` | Who the list applies to: `relay` (proxy-only nodes), `all`, or `none`. |
| `enrollment.token_ttl_secs` | `900` | Join token lifetime. Tokens are single use and bound to one node name. |
| `enrollment.cert_days` | `30` | Node certificate lifetime; nodes renew at two thirds. |
| `placement.capacity` / `quota` / `latency` / `locality` / `link` | `0.30 / 0.25 / 0.20 / 0.15 / 0.10` | Placement score weights (see ARCHITECTURE §5). |
| `data_dir` | `<config>/supervisor` | `supervisor.db` (state, events, audit, tokens). |
| `disconnect_grace_secs` | `600` | A disconnected node's sessions stay `unreachable` this long, then become `lost`. |
| `tasks.coordinator_agent` | `coordinator` | Agent profile used as the "main agent" for tasks. |
| `tasks.coordinator_node` | any | Pin coordinators to a node. |
| `tasks.max_concurrent_tasks` | `2` | Running tasks at once (+1 reserved for `urgent`). |
| `tasks.idle_grace_secs` | `30` | How long the cluster must be idle before a `when_free` task starts. |
| `tasks.max_nudges` | `2` | Re-prompts for a coordinator that stops without finishing. |
| `tasks.enabled` | `true` | Turn the scheduler off (tasks then only start via `ocl task run`). |

## providers.yaml

```yaml
providers:
  my-openrouter:
    base_url: https://openrouter.ai/api/v1   # OpenCode appends /chat/completions etc.
    api: openai          # openai | anthropic | google  (selects the AI SDK package)
    auth: { type: bearer }   # bearer | {type: header, name: x-api-key} | {type: query, name: key} | {type: none}
    headers: { HTTP-Referer: "https://example.org" }
    tier: standard       # best_effort marks free gateways without an SLA
    prices:              # USD per million tokens; used when the provider reports no cost
      qwen/qwen3-coder: { input_per_mtok: 0.20, output_per_mtok: 0.80 }
```

### Quotas (allowances) — `quota:`

```yaml
providers:
  kilo-free:
    quota:
      scope: ip                 # ip | key | account   (default: ip without a key, key otherwise)
      limits:                   # optional; provider signals take precedence
        - { kind: requests, max: 200, window: 1d, at: "00:00", tz: UTC }   # fixed window, resets daily at 00:00 UTC
        - { kind: requests, max: 20, window: 1m, rolling: true }           # rolling window
        - { kind: tokens, max: 1000000, window: 5h }
      learn: [headers, http_429]   # default
```

- **Buckets** are (provider, scope id, kind). The scope id is the route's egress IP (`ip`), a hash of the key (`key`), or the key's `account` label (`account`).
- **Learning:** keyproxy reads `x-ratelimit-{limit,remaining,reset}-{requests,tokens}` (OpenAI, Groq and most compatible gateways), `anthropic-ratelimit-*`, unsuffixed `x-ratelimit-*` (OpenRouter), `retry-after` and Gemini's `retryDelay`. A 429 marks the bucket exhausted until its reset.
- **Windows:** `window` takes `90s`, `5m`, `5h`, `1d` or `1h30m`; `at` is `HH:MM`; `tz` is `UTC` or an offset such as `+02:00`.
- **Persistence:** buckets survive node restarts (`node.db`) and are merged across nodes at the hub, so two machines behind one IP share a keyless bucket.

**Built-in providers:**
- **BYOK:** `openrouter`, `groq`, `openai`, `anthropic`, `gemini`.
- **Keyless or best-effort:** `kilo-free` (Kilo gateway's free models, the endpoint OpenKilo uses), `pollinations` (key optional), `keylessai` (third-party worker, no SLA).

An entry with the same id overrides a built-in.

## routes.yaml

```yaml
routes:
  direct:     { type: direct }
  home-proxy: { type: proxy, url: "socks5h://127.0.0.1:1080" }   # e.g. `ssh -D 1080 vps`
  via-vps:    { type: via_node, node: vps-de }                    # Phase 2
```

Routes only affect keyproxy's calls to the LLM provider, i.e. which IP the provider sees.

- **`direct`:** always exists.
- **`via_node`:** goes through the hub to a node with `role: relay` (or `relay: true`). The relay opens the TCP connection to the provider, and TLS stays end to end.
- **`proxy`:** any SOCKS5 or HTTP proxy.

## overrides.yaml

Written by the node when you change something live (`ocl route set`, `ocl quota set`, later the web UI); hot-reloaded. Delete it to drop every live change.

```yaml
routes:                      # agent -> route spec, replaces the agent's `route`
  builder: { prefer: via-relay-de, failover: [direct] }
quotas:                      # "this allowance resets at T / has N left"
  - { provider: kilo-free, scope_id: 203.0.113.7, resets_at: 1792804500000, remaining: 0, set_by: "operator:admin" }
```

## agents/<name>.yaml

```yaml
apiVersion: ocl/v1
kind: Agent
name: builder                       # [a-z0-9-]{1,48}; must be unique on the node
description: Writes small Python modules with pytest tests.
provider:
  id: openrouter                    # providers.yaml or built-in
  model: qwen/qwen3-coder           # the only model this agent may call (enforced by keyproxy)
  key: { ref: "env:OPENROUTER_API_KEY" }   # env:NAME | file:/path | systemd-creds:NAME | pass:entry; omit for keyless
  # key: { secret: openrouter }     # or an alias from node.yaml `secrets:` (hub templates)
  # key: { ref: "...", account: personal }   # label for account-scoped quotas
  context: 128000                   # advertised to OpenCode (default 128000)
  max_output: 16384
route: home-proxy                   # optional: one route name, or a plan:
# route: { prefer: direct, failover: [via-relay-de, home-proxy], on_exhausted: wait }   # wait | fail
role:
  system_prompt: "…"                # appended to OpenCode's own prompt
  system_prompt_file: prompts/builder.md
  opencode_agent: build             # OpenCode agent mode: build | plan | …
  preamble: "…"                     # template preamble, after the cluster preamble (variables allowed)
tools:                              # rendered into OpenCode's `permission` config
  edit: allow                       # allow | ask | deny
  bash: { default: ask, allow: ["pytest*", "python3 *"], deny: ["rm -rf *"] }
  webfetch: deny                    # default deny (containers have no network anyway)
  cluster: [send_message, read_inbox, list_agents]   # optional allowlist of cluster MCP tools
plugins: []                         # OpenCode plugins; must already be in the image
container:
  image: localhost/ocl-agent:1.19.4 # default: node runtime.image
  memory: 1g
  cpus: 1.0
  pids: 512
  egress: none                      # none (default) | full
  disk: 5g                          # soft limit for workspace + home: warn at 90%, interrupt at 100%
  tmp: 512m                         # size of the in-memory /tmp
workspace:
  mode: isolated                    # isolated | shared
  name: my-project                  # shared: same name ⇒ same directory (per node)
limits:
  max_sessions: 2                   # per node
  budget_usd: 1.00                  # per session; keyproxy refuses calls beyond it
  max_tokens: 500000                # per session (input+output+reasoning)
  wall_clock_secs: 1800             # the session is aborted after this
success:                            # reserved: ad-hoc sessions currently get a todo-only verdict;
  todos_required: true              # workflow steps define their own success block (below)
labels: { team: demo }
instances: { max: 4 }               # live instances across the whole cluster
overridable:                        # what a spawner may change per instance
  role_append: true                 #   extra instructions
  budget_usd: 3.00                  #   highest budget a spawner may set
  routes: [direct, via-relay-de]    #   preferred routes a spawner may pick
```

Spawning with overrides: `ocl spawn coder --append "…" --budget 2 --route via-relay-de`, the API's `overrides` field, or the `spawn_agent` tool's `role_append` / `budget_usd` / `route`. Anything outside `overridable` is refused.

## templates/ and preambles/ (hub)

`templates/*.yaml` use the agent format. The hub inlines `system_prompt_file` and pushes the library to every worker node on attach and whenever it changes. A node offers a template only when it can resolve its key, normally an alias (`key: { secret: openrouter }`) that the node maps in its own `secrets:`. Agents in a node's `agents/` win over templates with the same name. Nodes cache the library, so sessions keep working while the hub is down.

`preambles/cluster.md` replaces the built-in cluster preamble (a copy of the default is in `examples/remote/hub/preambles/cluster.md`). An agent's instructions are rendered in this order:
1. the cluster preamble;
2. the template's `role.preamble`;
3. the role prompt;
4. the orchestration briefing (e.g. for coordinators);
5. the spawner's `role_append`.

Variables are `{{instance.id}}`, `{{template}}`, `{{node}}`, `{{parent}}`, `{{task}}`, `{{budget}}`, `{{model}}`, `{{tools}}` and `{{question_mode}}`. Read a session's rendered instructions with `ocl show <session> --preamble`.

`ask` permissions make a session **blocked** until the operator answers with `ocl permit <session> <request> once|always|reject`.

## workflows/<name>.yaml

```yaml
apiVersion: ocl/v1
kind: Workflow
name: build-review
description: Builder implements, reviewer approves or sends it back.
workspace: { mode: shared, name: … }   # default: shared, one per run (run-<id>)
max_step_runs: 6                        # bounds review↔build loops
steps:
  - name: build
    agent: builder
    node: hub                           # optional pin
    prompt: |                           # {{input}} {{prev.output}} {{prev.step}} {{prev.todos}} {{run.id}} {{feedback}}
      Implement: {{input}}
      {{feedback}}
    success:
      todos: ["write implementation", "write tests", "run tests"]   # seeded into the prompt; must end completed
      todos_required: true
      max_nudges: 1                     # "continue with your open todos" re-prompts
      checks:
        - exit_code: "python3 -m pytest -q"   # runs in the session's container
        - text_contains: "DONE"               # the agent's final message must contain this
    next: review                        # default: the following step; `end` finishes
  - name: review
    agent: reviewer
    prompt: "Review: {{input}}\nBuilder said:\n{{prev.output}}"
    success:
      checks: [ { text_contains: "VERDICT: APPROVED" } ]
    on_fail: build                      # default: the run fails
```

**Verdicts:**
- **PASS:** all todos completed (seeded ones included) and every check passes.
- **INCOMPLETE:** open todos remain. The agent is nudged; once nudges are exhausted the verdict is FAIL.
- **FAIL:** a cancelled seeded todo, an error, or a failed check.
