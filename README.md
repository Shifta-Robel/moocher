# opencode-cluster

Run [OpenCode](https://opencode.ai) agents across several Linux machines as one cluster you can observe and control.

- **Supervisor** (hub): node registry, workflows (plan → build → review), a **task bucket** worked by a coordinator "main agent", REST/SSE API, hash-chained audit log. It holds no provider secrets.
- **opencode-node** (every machine, including the hub): runs each OpenCode session in a locked-down **Podman** container (no network, read-only config, all capabilities dropped). It injects provider keys through a local proxy, so agents only ever hold a revocable, budget-capped capability token. It also serves the `cluster_*` MCP tools agents use to message, delegate and report.
- **Dashboard**: a mobile-first web UI served by the supervisor (password login, HTTPS on your domain via Let's Encrypt, phone push), plus `ocl` (CLI) and `ocl tui`.
- **Remote machines** join with a one-time token (`ocl node invite`, then `opencode-node join`). Relay-only machines lend their IP to other nodes' LLM calls.

Status: **Phase 4 complete**:
- remote workers and IP-lending relays over mTLS, with join tokens, approval, revocation and a source allowlist;
- per-IP / key / account quotas with reset countdowns, and automatic failover to another IP;
- scored placement, a hub template library and resource stats;
- a content-addressed **artifact store** with lineage, and shared workspaces that work **across machines** (three-way sync);
- **questions** that climb the agent chain to you according to a question mode, with an assumption log;
- **dead agents** detected and recovered by cost (restart, resume on another machine, hand off) or the task **halted** for you, with one-command resume;
- **harvesting** of declared outputs and transcripts when an instance ends, and retention of its volumes;
- the **web dashboard** (every view, dark/light, phone layout), **Open in OpenCode** for any instance, notifications to your phone (Web Push), ntfy or a webhook, and a terminal dashboard.

Verified with seventeen end-to-end suites (`tests/e2e/`), including a real browser, an ACME CA, and load and chaos tests. Phase 5 adds the hardening: keyproxy usage metering, egress allowlists, hard disk quotas, event export and artifact expiry, and systemd/Quadlet units. See [docs/PHASE5.md](docs/PHASE5.md).

```bash
examples/quickstart/run-local.sh --with-worker   # full demo with a mock LLM
```

Docs:
- [Architecture](docs/ARCHITECTURE.md)
- [Quickstart](docs/QUICKSTART.md)
- [Usage guide](docs/USAGE.md) (extensive examples)
- [Config reference](docs/CONFIG.md)
- [Runbook](docs/RUNBOOK.md) (enroll machines, rotate credentials, debug)
- [Phase 4 report](docs/PHASE4.md)
- [Phase 3 report](docs/PHASE3.md)
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
