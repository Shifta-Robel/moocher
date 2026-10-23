# opencode-cluster

Run [OpenCode](https://opencode.ai) agents across several Linux machines as one cluster you can observe and control.

- **Supervisor** (hub): node registry, workflows (plan → build → review), a **task bucket** worked by a coordinator "main agent", REST/SSE API, hash-chained audit log. It holds no provider secrets.
- **opencode-node** (every machine, including the hub): runs each OpenCode session in a locked-down **Podman** container (no network, read-only config, all capabilities dropped). It injects provider keys through a local proxy, so agents only ever hold a revocable, budget-capped capability token. It also serves the `cluster_*` MCP tools agents use to message, delegate and report.
- **ocl**: a CLI over the API. The web UI (SolidJS, mobile-first) and TUI come in Phase 4.
- **Remote machines** join with a one-time token (`ocl node invite`, then `opencode-node join`). Relay-only machines lend their IP to other nodes' LLM calls.

Status: **Phase 2 complete**:
- remote workers and IP-lending relays over mTLS, with join tokens, approval, revocation and a source allowlist;
- per-IP / key / account quotas with reset countdowns, and automatic failover to another IP;
- scored placement, a hub template library and resource stats.

Verified with six end-to-end suites (`tests/e2e/`, 109 checks). Artifacts and question escalation are Phase 3, UI and auth Phase 4, hardening Phase 5.

```bash
examples/quickstart/run-local.sh --with-worker   # full demo with a mock LLM
```

Docs:
- [Architecture](docs/ARCHITECTURE.md)
- [Quickstart](docs/QUICKSTART.md)
- [Usage guide](docs/USAGE.md) (extensive examples)
- [Config reference](docs/CONFIG.md)
- [Runbook](docs/RUNBOOK.md) (enroll machines, rotate credentials, debug)
- [Phase 2 report](docs/PHASE2.md)
- [Phase 1 report](docs/PHASE1.md)
- [Phase 1.5 design additions](docs/PHASE1_5.md) (templates, quotas per IP, questions, health, UI)
- [Phase 0 evaluation](docs/PHASE0.md)

| Binary | Crate |
|---|---|
| `ocl-supervisor` | `supervisor/` |
| `opencode-node` | `node-daemon/` |
| `ocl-bridge` | `bridge/` (static, runs inside agent containers) |
| `ocl` | `cli/` |
