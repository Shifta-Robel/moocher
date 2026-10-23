# opencode-cluster

Run [OpenCode](https://opencode.ai) agents across several Linux machines as one cluster you can observe and control.

- **Supervisor** (hub): node registry, workflows (plan → build → review), a **task bucket** worked by a coordinator "main agent", REST/SSE API, hash-chained audit log. It holds no provider secrets.
- **opencode-node** (every machine, including the hub): runs each OpenCode session in a locked-down **Podman** container (no network, read-only config, all capabilities dropped). It injects provider keys through a local proxy, so agents only ever hold a revocable, budget-capped capability token. It also serves the `cluster_*` MCP tools agents use to message, delegate and report.
- **ocl**: a CLI over the API. The web UI (SolidJS, mobile-first) and TUI come in Phase 4.

Status: **Phase 1 complete** (single machine, verified end to end). Remote nodes with mTLS are Phase 2, artifacts Phase 3, UI and auth Phase 4, hardening Phase 5.

```bash
examples/quickstart/run-local.sh --with-worker   # full demo with a mock LLM
```

Docs:
- [Architecture](docs/ARCHITECTURE.md)
- [Quickstart](docs/QUICKSTART.md)
- [Usage guide](docs/USAGE.md) (extensive examples)
- [Config reference](docs/CONFIG.md)
- [Phase 1 report](docs/PHASE1.md)
- [Phase 1.5 design additions](docs/PHASE1_5.md) (templates, quotas per IP, questions, health, UI)
- [Phase 0 evaluation](docs/PHASE0.md)

| Binary | Crate |
|---|---|
| `ocl-supervisor` | `supervisor/` |
| `opencode-node` | `node-daemon/` |
| `ocl-bridge` | `bridge/` (static, runs inside agent containers) |
| `ocl` | `cli/` |
