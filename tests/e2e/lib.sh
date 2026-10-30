# Shared helpers for the e2e suites (sourced). They run the real binaries with
# the `process` runtime (no Podman needed) and a scripted mock LLM; "remote"
# nodes are separate config dirs that enroll and talk mTLS to 127.0.0.1.
#
# Settings (environment):
#   MOO_BIN            built binaries        (default: <repo>/target/debug)
#   MOO_OPENCODE_BIN   opencode 1.19.x       (default: `opencode` on PATH)
#   MOO_E2E_DIR        scratch directory     (default: /tmp/moo-e2e). Keep it short:
#                      session sockets must fit in 107 bytes.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN=${MOO_BIN:-$REPO/target/debug}
OC=${MOO_OPENCODE_BIN:-$(command -v opencode || true)}
[ -x "$OC" ] || { echo "opencode not found: install opencode-ai@1.19.4 or set MOO_OPENCODE_BIN" >&2; exit 2; }
S=${MOO_E2E_DIR:-/tmp/moo-e2e}
mkdir -p "$S"
export MOO_BRIDGE=$BIN/moo-bridge
W=$S/work
O="$BIN/moo"
PIDS=()
cleanup() {
  # Stop live sessions through the API first, so their process trees go too
  # (stopping a node daemon leaves sessions running, by design).
  if [ -n "${MOO_TOKEN:-}" ]; then
    for s in $($O sessions --json 2>/dev/null | python3 -c 'import sys,json;[print(s["id"]) for s in json.load(sys.stdin) if s["state"] not in ("stopped","lost")]' 2>/dev/null); do
      $O stop "$s" >/dev/null 2>&1 || true
    done
  fi
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  pkill -f "$W/" 2>/dev/null || true
  # Session bridges (and their opencode) are named by the per-node run dirs.
  pkill -f "unix:$S/r[0-9a-f]{4}/" 2>/dev/null || true
  sleep 1
}
trap cleanup EXIT
# Session sockets must fit in 107 bytes: short per-node run dirs under $S.
run_dir() { echo "$S/r$(echo "$1" | md5sum | cut -c1-4)"; }
# Read all of stdin, then match (grep -q exits early, which breaks pipefail).
contains() { grep -c -- "$1" >/dev/null; }
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0
expect() { # expect "description" cmd... : command must succeed
  local d=$1; shift
  if "$@" >/dev/null 2>&1; then pass "$d"; else fail "$d"; fi
}
expect_fail() {
  local d=$1; shift
  if "$@" >/dev/null 2>&1; then fail "$d"; else pass "$d"; fi
}
wait_for() { # wait_for secs "description" cmd...
  local n=$1 d=$2; shift 2
  for _ in $(seq 1 "$n"); do
    if "$@" >/dev/null 2>&1; then pass "$d"; return 0; fi
    sleep 1
  done
  fail "$d (timed out after ${n}s)"
}

# hub: supervisor + its own node (process runtime)
port_free() { python3 -I -c "import socket,sys;s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind(('127.0.0.1',int(sys.argv[1])))" "$1" 2>/dev/null; }
start_hub() {
  for p in 8099 7700 "${LINK_PORT:-7443}"; do
    for _ in $(seq 1 30); do port_free "$p" && break; sleep 1; done
  done
  # A previous run's stragglers may still be writing; move it aside first.
  if [ -e "$W" ]; then mv "$W" "$W.old.$$"; (rm -rf "$W.old.$$" 2>/dev/null &); fi
  mkdir -p "$W/run" "$W/scripts"
  # Mock scripts are read per request: suites add their own with mock_script.
  cp "$REPO"/examples/mock-llm/scripts/*.json "$W/scripts/"
  cp -r "$REPO/examples/quickstart/hub" "$W/hub"
  cat > "$W/hub/node.yaml" <<EOF
name: hub
supervisor: unix://$W/run/supervisor.sock
max_sessions: 6
runtime: { kind: process, opencode_bin: $OC, run_dir: $(run_dir hub) }
labels: { role: hub }
${HUB_NODE_YAML:-}
EOF
  cat >> "$W/hub/supervisor.yaml" <<EOF
link_socket: $W/run/supervisor.sock
link_listen: 127.0.0.1:${LINK_PORT:-7443}
public_names: [localhost, 127.0.0.1]
${ENROLL_YAML:-}
EOF
  export MOCK_API_KEY="mock-key-$RANDOM$RANDOM"
  # shellcheck disable=SC2086
  python3 -I "$REPO/examples/mock-llm/mock_llm.py" --port 8099 --scripts "$W/scripts" --require-key "$MOCK_API_KEY" ${MOCK_ARGS:-} > "$W/mock.log" 2>&1 &
  PIDS+=($!)
  "$BIN/moocher-hub" --config-dir "$W/hub" init > "$W/init.txt"
  export MOO_TOKEN="$(cat "$W/hub/cli-token")"
  "$BIN/moocher-hub" --config-dir "$W/hub" run > "$W/supervisor.log" 2>&1 &
  PIDS+=($!)
  sleep 1
  "$BIN/moocher-node" --config-dir "$W/hub" run > "$W/node-hub.log" 2>&1 &
  PIDS+=($!)
  sleep 2
}

# join_node NAME [--relay]: invite on the hub, `moocher-node join` in $W/NAME
join_node() {
  local name=$1; shift
  local cmd
  cmd=$($O node invite "$name" "$@" --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["command"])')
  # shellcheck disable=SC2086
  "$BIN/moocher-node" --config-dir "$W/$name" ${cmd#moocher-node }
}

# make_worker NAME: after join, give the worker agents and a process runtime
make_worker() {
  local name=$1
  mkdir -p "$W/$name/agents"
  cp "$W/hub/providers.yaml" "$W/$name/"; cp -r "$W/hub/prompts" "$W/$name/"
  cp "$W/hub/agents/builder.yaml" "$W/hub/agents/reviewer.yaml" "$W/$name/agents/"
  python3 -I - "$W/$name/node.yaml" "$OC" "$(run_dir "$name")" <<'PY'
import sys
p, oc, run = sys.argv[1:]
lines = [l for l in open(p).read().splitlines() if not l.startswith("runtime:")]
lines.append(f"runtime: {{ kind: process, opencode_bin: {oc}, run_dir: {run} }}")
open(p, "w").write("\n".join(lines) + "\n")
PY
}

start_node() { # start_node NAME
  "$BIN/moocher-node" --config-dir "$W/$1" run > "$W/node-$1.log" 2>&1 &
  PIDS+=($!)
  eval "PID_$(echo "$1" | tr - _)=$!"
}

node_field() { # node_field NAME FIELD
  $O nodes --json | python3 -c "import sys,json;n=[x for x in json.load(sys.stdin) if x['name']=='$1'];print(n[0]['$2'] if n else '')"
}
is_connected() { [ "$(node_field "$1" connected)" = "True" ]; }
is_disconnected() { [ "$(node_field "$1" connected)" = "False" ]; }

# mock_script NAME < json: a scripted model for this suite (model name = NAME).
mock_script() { cat > "$W/scripts/$1.json"; }

# mock_prices MODEL...: give scripted models a price on every config dir
# (recovery costs are estimated from it). Run before make_worker copies providers.yaml.
mock_prices() {
  python3 -I - "$W/hub/providers.yaml" "$@" <<'PY'
import sys
p, models = sys.argv[1], sys.argv[2:]
s = open(p).read()
add = "".join(f"      {m}: {{ input_per_mtok: 0.50, output_per_mtok: 1.50 }}\n" for m in models if f"      {m}:" not in s)
s = s.replace("    prices:\n", "    prices:\n" + add, 1)
open(p, "w").write(s)
PY
}

# agent_yaml DIR NAME MODEL [extra yaml]: an agent profile using the mock.
agent_yaml() {
  mkdir -p "$1/agents"
  cat > "$1/agents/$2.yaml" <<EOF
name: $2
description: e2e agent $2
provider: { id: mock, model: $3, key: { ref: "env:MOCK_API_KEY" } }
tools: { edit: allow, bash: { default: ask, allow: ["python3 *", "ls*", "cat *", "sleep *"] } }
limits: { max_sessions: 4 }
${4:-}
EOF
}

# sess EXPR ID: a field of a session (python expression on the dict `s`).
sess() { $O show "$2" --json | python3 -c "import sys,json;s=json.load(sys.stdin);print($1)"; }
sess_state() { sess 's["state"]' "$1"; }
spawn_id() { $O spawn "$@" --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'; }
# Usable inside `wait_for ... bash -c "..."` too.
export O W
export -f sess sess_state
# Stop every live session (between scenarios, so node capacity is free again).
stop_all() {
  for s in $($O sessions --json | python3 -c 'import sys,json;[print(s["id"]) for s in json.load(sys.stdin) if s["state"] not in ("stopped","lost","dead")]'); do
    $O stop "$s" >/dev/null 2>&1 || true
  done
}
