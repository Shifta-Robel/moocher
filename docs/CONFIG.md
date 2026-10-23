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
```

**Hot reload:** the node watches `node.yaml`, `providers.yaml`, `routes.yaml`, `agents/`, `prompts/` and `workflows/`.
- **Apply immediately:** provider, route, key and limit changes (keyproxy reads them per request).
- **Apply to new sessions:** role, tools and container changes.
- **Errors:** a file that fails to parse is reported and ignored, and the last good config stays active.

## node.yaml

| Key | Default | Meaning |
|---|---|---|
| `name` | — | Node name, `[a-z0-9-]{1,48}`, unique in the cluster. |
| `supervisor` | — | `unix:///run/ocl/supervisor.sock` (same machine). `https://hub:7443` arrives in Phase 2 (mTLS). |
| `max_sessions` | `6` | Concurrent live sessions on this node (~420 MB RAM each). |
| `relay` | `false` | Allow `via_node` egress through this machine (Phase 2). |
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
  context: 128000                   # advertised to OpenCode (default 128000)
  max_output: 16384
route: home-proxy                   # optional
role:
  system_prompt: "…"                # appended to OpenCode's own prompt
  system_prompt_file: prompts/builder.md
  opencode_agent: build             # OpenCode agent mode: build | plan | …
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
```

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
