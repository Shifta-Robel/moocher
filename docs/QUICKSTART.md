# Quickstart

First the hub (supervisor + its own node), with an optional second local node, all on one Linux machine. Then [remote machines](#adding-remote-machines-workers-and-relays) over mTLS.

## Prerequisites

- Linux with **Podman ≥ 4.4** (rootful or rootless; rootless needs cgroup v2 delegation for memory limits)
- Rust toolchain ≥ 1.80 with the musl target: `rustup target add x86_64-unknown-linux-musl`
- `python3` (only for the mock LLM)
- With `runtime: process` (dev only, no containers), workflow checks run on the host, so the demo also needs `pytest` installed there. The agent image already includes it.

## One command: the full demo

```bash
examples/quickstart/run-local.sh --with-worker
```

The script:
1. builds the binaries and the agent image `localhost/ocl-agent:1.19.4`;
2. starts the mock LLM, the supervisor, the hub's node and a second node `worker-a`;
3. runs the **build-review workflow**;
4. puts three tasks in the bucket (urgent, when_free, on_demand);
5. sends a mid-run command to the builder that the coordinator spawned;
6. prints verdicts, token usage, cost, the resulting file and the audit-chain check.

Add `--keep` to leave everything running afterwards.

If `/run` isn't writable for your user, set `OCL_RUN_DIR`, e.g. `OCL_RUN_DIR=$XDG_RUNTIME_DIR/ocl`. Behind a TLS-intercepting proxy, build the image yourself first (see `images/agent/Containerfile` for the `/run/extra-ca.crt` option).

## Step by step

```bash
cargo build --release
cargo build --release -p ocl-bridge --target x86_64-unknown-linux-musl
podman build -t localhost/ocl-agent:1.19.4 -f images/agent/Containerfile .

mkdir -p ~/.opencode-cluster && cp -r examples/quickstart/hub/* ~/.opencode-cluster/
export OCL_BRIDGE=$PWD/target/x86_64-unknown-linux-musl/release/ocl-bridge

# 1. Hub: supervisor (prints an admin token, saved to ~/.opencode-cluster/cli-token)
target/release/ocl-supervisor init
target/release/ocl-supervisor run &

# 2. Hub's own node. The provider key lives only in this process's environment.
export MOCK_API_KEY=mock-provider-key-123
python3 examples/mock-llm/mock_llm.py --require-key "$MOCK_API_KEY" &
target/release/opencode-node check     # validates agents, secrets, routes
target/release/opencode-node run &

# 3. Use it
O=target/release/ocl
$O agents
$O workflow run build-review "Write greet(name) in greet.py with pytest tests"
$O status                       # nodes, sessions (state, todos, tokens, cost), tasks
$O task add "Greeting module" -b "Create greet(name) with tests, reviewed" --label urgent
$O send <session> "Please also add a docstring"        # mid-run command
$O events -f                    # live event log
$O audit --verify
```

## Using a real provider instead of the mock

Edit `~/.opencode-cluster/agents/builder.yaml`. The agent is restarted with the new config the next time it is spawned (hot reload):

```yaml
provider:
  id: openrouter
  model: qwen/qwen3-coder
  key: { ref: "env:OPENROUTER_API_KEY" }   # or systemd-creds:openrouter, file:/path, pass:entry
```

Then export `OPENROUTER_API_KEY` for `opencode-node`, and **only** for it. Keyless alternatives:
- `provider: { id: kilo-free, model: <free model> }` (no key)
- `pollinations` (key optional)

Run `opencode-node check` to confirm everything resolves.

## The task bucket

```bash
$O task add "Refactor utils" --label when_free        # runs when the cluster is idle
$O task add "Fix prod bug" -b "details…" --label urgent  # starts right away
$O task add "Release notes" --label on_demand         # only when you say so
$O task order 7 3 5        # put these first, in this order (the web UI will drag-and-drop)
$O task run 5              # start now regardless of label
$O task cancel 7
```

Tag a task `-t workflow:build-review` to run a workflow instead of a coordinator.

## Adding remote machines (workers and relays)

On the hub, enable the mTLS link in `supervisor.yaml` and restart the supervisor:

```yaml
link_listen: 0.0.0.0:7443
public_names: [hub.example.org]
```

Then, per machine:

```bash
# hub
ocl node invite vps-fi                       # prints: opencode-node join https://hub.example.org:7443 --token ocljoin.…
# the new machine (binaries + Podman + agent image installed)
opencode-node join https://hub.example.org:7443 --token ocljoin.…
opencode-node run                            # or enable the systemd unit
# hub
ocl node approve vps-fi
ocl nodes                                    # vps-fi  worker  approved  connected  14ms  198.51.100.4 …
ocl spawn builder --node vps-fi -p "…"       # or leave --node out and let placement choose
```

**A machine that only lends its IP** to other nodes' LLM calls:
1. Allow its address on the hub: `ocl allow add <ip>`.
2. Invite it with `ocl node invite relay-home --relay`, join, and approve as above.
3. Refer to it in workers' `routes.yaml` as `{ type: via_node, node: relay-home }`.

To switch IP when a free allowance runs out, give the agent a plan: `route: { prefer: direct, failover: [via-home] }`. Then watch:
- `ocl quotas`: allowances and reset countdowns;
- `ocl ips`: who shares which IP;
- `ocl stats`: CPU, memory and disk.

Full procedures and failure modes: [RUNBOOK.md](./RUNBOOK.md). Example configs: [`examples/remote/`](../examples/remote/).

The same scenarios run as automated tests on one machine: `tests/e2e/run-all.sh` (see `tests/e2e/README.md`).

## Troubleshooting

| Symptom | Check |
|---|---|
| `no connected node can run agent X` | `ocl agents`: is it `READY=yes`? If not, `opencode-node check` shows the reason (secret, provider, route). |
| Session stuck in `starting` | `podman logs ocl-<session>`; `<data>/instances/<session>/home/.local/share/opencode/log/`. A cold start takes ~35 s. |
| Session `blocked` | It's waiting on a permission (`bash: ask`). `ocl show <session>` gives the request id; then `ocl permit <session> <request> once`. |
| Session `error` with `[opencode-cluster] …` | The message came from keyproxy: budget, token cap, model allowlist, or unresolvable credential. |
| Node logs `supervisor link down` | The supervisor isn't running, or the socket path differs: compare `node.yaml: supervisor` with `supervisor.yaml: link_socket`. Events are buffered meanwhile. |
| `permission denied` on `/run/ocl` | Set `runtime.run_dir` in `node.yaml` and `link_socket` in `supervisor.yaml` to a directory you own (short: socket paths must fit in 107 bytes). |
| Remote node problems (join, approval, certificates, relays, quotas) | [RUNBOOK.md §6](./RUNBOOK.md#6-debugging) |
