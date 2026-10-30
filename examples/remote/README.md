# Example: hub + remote worker + relay

Copy these into `~/.moocher/` on each machine and adjust names,
addresses and secret references. See docs/RUNBOOK.md for the enrollment steps
and docs/CONFIG.md for every key.

| Directory | Machine | What it shows |
|---|---|---|
| `hub/` | the always-on hub | mTLS link listener, enrollment allowlist, placement weights, a template library with secret aliases, the cluster preamble |
| `worker/` | a VPS or laptop that runs agents | `supervisor: https://…`, secret aliases, per-IP and per-account quotas, failover routes through relays |
| `relay/` | a machine that only lends its IP | `role: relay`, allowed destination hosts |

`moocher-node join` writes `node.yaml` (name, supervisor URL, role) and
`pki/` for you; the files here show what you add afterwards.
