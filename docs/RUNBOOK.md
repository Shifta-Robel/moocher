# Operations runbook

Commands run on the hub with `ocl` unless they say otherwise. Every control action below is written to the audit log (`ocl audit`).

## 1. Turn on remote nodes (once)

In the hub's `supervisor.yaml`:

```yaml
link_listen: 0.0.0.0:7443
public_names: [hub.example.org]      # what machines will dial; goes into the link certificate
enrollment:
  require_approval: true
  enforce_for: relay                 # relay | all | none
```

- **Restart:** `systemctl restart ocl-supervisor`.
- **What happens:**
  - On first start the hub creates its CA under `<data>/supervisor/pki/` (Ed25519; `ca.key` is 0600). Back this directory up; see §4.4.
  - The log line `remote node link (mTLS) enabled` shows the CA fingerprint.
- **Firewall:** open TCP 7443 to the hub only. Workers and relays dial out, so they need no inbound ports.

## 2. Enroll a machine

### 2.1 A worker (runs agents)

```bash
# hub
ocl node invite vps-fi --label region=eu-north
#   Join token for vps-fi (worker), single use, valid until 14:05:12:
#     opencode-node join https://hub.example.org:7443 --token ocljoin.….…
```

On the new machine:
1. Install `opencode-node` and `ocl-bridge`, plus Podman and the agent image (see QUICKSTART).
2. Run the printed command:
   ```bash
   opencode-node join https://hub.example.org:7443 --token ocljoin.…
   ```
   - It generates the node key locally; the key never leaves the machine.
   - It verifies the hub against the CA fingerprint inside the token, then stores `pki/` and writes `node.yaml` if there is none.
3. Add `agents/` or `secrets:` aliases for the hub's templates (examples in `examples/remote/worker/`).
4. Start it: `systemctl enable --now opencode-node`.

Back on the hub:
```bash
ocl node approve vps-fi
ocl nodes          # vps-fi  worker  approved  connected  12ms  198.51.100.4 ...
```

The node retries every 10 s while it is pending, so it connects within seconds of approval. To skip approval, set `require_approval: false`.

### 2.2 A relay (only lends its IP)

```bash
ocl allow add 192.0.2.44 --note "home router"    # relays must come from an allowed source
ocl node invite relay-home --relay
```

- **On the relay:** run the same `opencode-node join …` command. Podman isn't needed.
- **Its node.yaml:** `role: relay`, `relay_ports: [443]` and ideally `relay_allow_hosts` listing your providers' API hosts.
- **On the hub:** run `ocl node approve relay-home`.
- **Using it:** workers refer to it as a route:
  ```yaml
  # worker routes.yaml
  routes:
    via-home: { type: via_node, node: relay-home }
  ```
  An agent uses it as `route: via-home`, or in a failover plan (`route: { prefer: direct, failover: [via-home] }`).
- **Check:** `ocl ips` lists each route's public IP and who shares it.

### 2.3 Allowed sources

```bash
ocl allow ls
ocl allow add 198.51.100.0/24 --note "VPS provider range"
ocl allow rm 192.0.2.44        # relays from that address are disconnected at once
```

With `enforce_for: all`, every remote node needs an allowed source, not just relays.

## 3. Remove or replace a machine

| Goal | Command |
|---|---|
| Cut a node off now | `ocl node revoke vps-fi`: its certificates are revoked and the link is closed. A revoked node retries only every 10 min and is logged once per 10 min. |
| Forget a dead node | `ocl node forget vps-fi` (it must be disconnected). Session history is kept. |
| Re-install a machine under the same name | `ocl node revoke vps-fi` (if still enrolled), then `ocl node invite vps-fi` and join again. The new certificate replaces the old ones. |

Sessions on a node that disappears become `unreachable`, then `lost` after `disconnect_grace_secs` (default 10 min).

## 4. Rotate credentials

### 4.1 Node certificates

- **Automatic:** a node renews its certificate at two thirds of `cert_days` (day 20 of 30) over its live link, with a new key each time. The new certificate is used from the next reconnect.
- **Check:** `ocl nodes --json | jq '.[] | {name, cert_expires_at}'`.
- **Force:** to force a new identity now, re-enroll (§3).

### 4.2 Provider keys

- **Where keys live:** only on the node, in the secret backend (`env:`, `file:`, `systemd-creds:`, `pass:`).
- **No restart needed:** keyproxy reads the secret per request. So for systemd credentials:
  ```bash
  sudo systemd-creds encrypt --name=openrouter - /etc/ocl/creds/openrouter.cred   # paste the new key
  sudo systemctl restart opencode-node      # reloads credentials; containers keep running
  ```
- **For `file:` refs:** replace the file; no restart is needed at all.

### 4.3 Operator API tokens

```bash
ocl token create ci --scope read     # shown once
ocl token ls
ocl token revoke ci
```

### 4.4 The cluster CA

There is no in-place CA rotation in v1. To replace the CA (e.g. after `ca.key` leaked):
1. Stop the supervisor.
2. Move `<data>/supervisor/pki/` aside.
3. Start the supervisor; it creates a new CA and link certificate.
4. Re-enroll every remote node (§3). Their old certificates no longer verify.

The unix-socket hub node is unaffected.

## 5. Quotas and IPs

| Task | Command |
|---|---|
| See allowances and reset countdowns | `ocl quotas` or `ocl quotas kilo-free` |
| See which agents share an IP | `ocl ips` |
| "This IP's allowance on vps-fi resets at 03:15 and is used up" | `ocl quota set kilo-free --scope-id 203.0.113.7 --node vps-fi --resets-at 03:15 --remaining 0` (`03:15` is local time, `03:15Z` is UTC, `+2h` is relative) |
| Undo | `ocl quota clear kilo-free --scope-id 203.0.113.7` |
| Move an agent to another IP now | `ocl route set vps-fi free-coder --prefer via-home --failover direct`, then `ocl route clear vps-fi free-coder` to undo |
| Re-learn egress IPs | `ocl node probe vps-fi` |

Live changes go into the node's `overrides.yaml`, never into your versioned files.

## 6. Questions, halts and harvests

### 6.1 The question inbox

```bash
ocl questions                     # waiting on you first: importance, then how many wait, then age
ocl answer <q-id> "text"          # or --choice N, --decide (let the agent decide), --reject
ocl questions mode                # cluster mode + per-task modes
ocl questions mode vital_only     # live; persists across restarts (overrides questions.mode)
ocl notifications --unread --read # what you missed, then mark it read
ocl assumptions --task 7          # what agents decided without you
```

- **A question sits with an agent:** it moves up by itself after `questions.answer_timeout_secs` (default 300). To take it over, answer it yourself: the operator may answer any open question.
- **Too many questions reach you:** switch to `vital_only` or `autonomous` (per task with `--task`).
- **Too few reach you:** use `ask_me_everything` while you watch a task.

### 6.2 A halted task

```bash
ocl halts                         # reason, what it was doing, spend, priced options
ocl recover <h-id> restart        # or resume_elsewhere [--node X], handoff, abandon
```

- **Choosing an option:**
  - `restart` keeps the agent's history and is usually cheapest.
  - Use `resume_elsewhere` when the node is unhealthy or gone.
  - Use `handoff` when the agent kept failing the same way.
  - `abandon` fails the task (or step) with the death reason.
- **Halted sessions keep their containers.** `ocl task cancel <id>` ends them, and their outputs are harvested.
- **Before restarting an `oom` death,** raise the template's `container.memory`. A restart with the same limit dies again.
- **Lost nodes:** the replacement starts from the latest checkpoint (`checkpoint_every_turns`). `ocl harvests --session <id>` says what was unrecoverable.

### 6.3 Harvests and disk

- **See what was collected:** `ocl harvests [--session X]`, then `ocl artifact get <ref> -o <dir>`.
- **Volumes of stopped instances** stay for `retain:` (default 24 h) under `data/instances/<id>/` and `data/workspaces/<id>/`; the node removes them when due. To free disk now, set `retain: never` on the template (it applies at the next stop), or delete those directories by hand for sessions that are `stopped`.
- **Artifact blobs** live in the hub's `<data>/artifacts/` (back it up with the database). Nodes cache them in `data/cas/` up to `artifact_cache`.

## 7. The dashboard: logins, HTTPS, attach, push

| Situation | Fix |
|---|---|
| Locked out of your account (`too many failed logins`) | Wait it out, or on the hub `ocl-supervisor user set <name>` (sets a new password and clears the lockout). |
| Lost the authenticator app | `ocl-supervisor user totp <name> --off`, log in, then `user totp <name>` again. |
| Someone hammers the login page | Every failure is in `ocl audit` (`login.failed` with the IP); lockouts raise vital notifications. Put the hub behind a firewall or VPN if it does not need to be public. |
| Log every browser out | Change the password (`ocl-supervisor user set`): it ends that user's sessions. |
| ACME order fails | The supervisor log says why; a `tls.acme_failed` notification is raised. Check that the domain resolves to the hub, that port 80 reaches `http01_listen`, and try the staging directory first. Orders retry with backoff (1 min → 6 h). |
| The certificate is not renewing | It renews 30 days before expiry, checked twice a day; `<data>/acme/cert.pem` shows the current one (`openssl x509 -noout -enddate -in …`). |
| Replace your own certificate | Overwrite `cert_file` / `key_file`: picked up within a minute (an invalid pair is refused and the old one kept). |
| Open in OpenCode: "Link expired" | The ticket is single use and lives 60 s: click **Open in OpenCode** again. |
| Open in OpenCode: "Not attached" / 401 | The 8-hour attach cookie expired or belongs to another instance: open it again from the dashboard. |
| Open in OpenCode does nothing | `attach_listen` is not set (the button is hidden then), or the port is firewalled; behind a proxy, set `attach_url` and proxy that port too (WebSocket upgrades included). |
| Push never arrives | Push needs HTTPS (or localhost). Use **Send a test** on the Notifications page. If the subscription is gone (`404/410` from the push service) the hub drops it: enable push again. On iOS the dashboard must be added to the home screen. |
| ntfy / webhook silent | Only `warning` and `vital` go out by default, and nothing but `vital` during quiet hours; check `notifications.rules`. Failures are in the supervisor log (`ntfy refused…`). |

## 8. Debugging

Logs are JSON: `journalctl -u opencode-node -o cat | jq -r '.fields.message'`. Raise verbosity with `OCL_LOG=debug` (or `OCL_LOG=info,opencode_node::gateway=debug`).

| Symptom | Cause and fix |
|---|---|
| `opencode-node join` says `enrollment refused: unknown join token` / `already used` / `expired` | Tokens are single use and last `token_ttl_secs` (15 min). Run `ocl node invite <name>` again. |
| `join` fails with `hub CA fingerprint does not match the join token` | You are not talking to your hub (DNS, proxy, or a TLS-intercepting middlebox), or the hub's CA was replaced after the invite. Check the URL, then invite again. |
| `join` fails with a certificate name error | The hub's certificate does not contain the name you dialed. Add it to `public_names` and restart the supervisor; it reissues the link certificate. |
| `source address … is not in the allowed sources list` | `ocl allow add <ip>`. The hub logs the address it saw; behind NAT that is the public address. |
| Node log: `awaiting operator approval` | `ocl node approve <name>`. |
| Node log: `certificate for X has been revoked` / `unknown client certificate` | Re-enroll (§3). |
| Node log: `certificate … expired` | The node was offline longer than its certificate's life. Re-enroll. Also check the clock (NTP): large skew breaks TLS. |
| Node starts then exits: `runtime.run_dir … is too long for unix sockets` | Session sockets must fit in 107 bytes. Use a short `runtime.run_dir` (default `/run/ocl`). |
| Session `running` with current task `retrying (attempt N…)` | OpenCode is retrying a failed provider call; the message after the colon says why, e.g. `upstream unreachable: relay relay-home: … port 8099 is not in this relay's relay_ports [443]`. Fix the relay's `relay_ports` / `relay_allow_hosts`, or bring the relay back. The session continues by itself. |
| Session `parked`, current task `resets in 42m: …` | Every route's allowance for that provider is used up. It resumes by itself after the reset. To move it now, add a failover route or a relay. If your provider resets earlier than learned, set it with `ocl quota set`. |
| `ocl ips` shows `?` for a route | The echo probe failed (see NOTE). Behind a relay, allow `api.ipify.org` in `relay_allow_hosts`, or set `ip_echo` to a service you trust. Until then, quotas for that route are counted under `route:<node>/<route>`. |
| `ocl templates` lists `not on <node>: secret alias does not resolve` | Add the alias to that node's `node.yaml` `secrets:` (`openrouter: { ref: "env:OPENROUTER_API_KEY" }`). The node reloads it automatically. |
| A template is missing on a node | That node has an agent with the same name in `agents/` (local wins), or `hub_templates: false`. |
| Spawn refused: `… does not allow …` / `budget_usd … outside` | The template's `overridable` does not permit that override. |
| Session error `disk limit exceeded: …` | `container.disk` reached. Clean the workspace, raise the limit, or stop the session. OpenCode's home alone takes ~120 MB, so limits below ~500 MB are tight. |
| Hub restarted and a remote node shows `disconnected` | Nodes back off up to 60 s; it reconnects and replays its buffered events (`ocl events`). |
| `ocl` prints `contacting supervisor … Connection refused` | The supervisor is down or `OCL_URL` is wrong. |
| Session `dead`, `exit:137` | Killed: often memory (`oom` when Podman says so) or a manual kill. See §6.2. |
| Session `dead`, `wall_clock` | `limits.wall_clock_secs` reached. It is not retried automatically; `ocl recover <id> restart` gives it a fresh wall clock. |
| `artifact_put` says `… resolves outside the workspace` / `is a symlink` | The agent tried to share a link; links are never followed. Share the file itself. |
| Workspace files `*.theirs` appear | A cross-machine sync found the same file changed on both machines (§ USAGE 9). Reconcile and delete the `.theirs` file. |
| `upload refused: … artifacts.max_size` | Raise `artifacts.max_size` on the hub, or share a smaller tree. |
| A new instance takes ~30 s to start on a dev host | Fixed by the node pre-seeding OpenCode's model catalog; if it comes back, check the OpenCode log for a network fetch at boot (`process` runtime only; containers have no network). |

Health at a glance: `ocl status`, `ocl nodes` (link RTT, enrollment, IP), `ocl stats` (CPU/memory/disk per host and instance), `ocl quotas`, `ocl questions`, `ocl halts`, `ocl notifications --unread`, and `/metrics` (`ocl_relay_*`, `ocl_sessions{state}`, …).
