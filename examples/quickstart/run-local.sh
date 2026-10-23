#!/usr/bin/env bash
# Phase 1 quickstart: hub (supervisor + its own node) on this machine, an
# optional second local node, a mock LLM, and the two demos:
#   1. workflow build-review (builder -> reviewer handoff with verdicts)
#   2. task bucket: an urgent task handled by the coordinator, with a mid-run
#      operator message to the builder it spawns.
#
# Usage: examples/quickstart/run-local.sh [--with-worker] [--keep]
# Needs: cargo, podman, python3, and the agent image (built below if missing).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="${OCL_DEMO_DIR:-$REPO/target/demo}"
RUN_DIR="${OCL_RUN_DIR:-${XDG_RUNTIME_DIR:-/run}/ocl}"
IMAGE="localhost/ocl-agent:1.19.4"
WITH_WORKER=0
KEEP=0
for a in "$@"; do
  case "$a" in
    --with-worker) WITH_WORKER=1 ;;
    --keep) KEEP=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "building"
cargo build --release --manifest-path "$REPO/Cargo.toml" -p ocl-supervisor -p opencode-node -p ocl
cargo build --release --manifest-path "$REPO/Cargo.toml" -p ocl-bridge --target x86_64-unknown-linux-musl
BIN="$REPO/target/release"
export OCL_BRIDGE="$REPO/target/x86_64-unknown-linux-musl/release/ocl-bridge"
if ! podman image exists "$IMAGE"; then
  say "building agent image $IMAGE"
  podman build -t "$IMAGE" -f "$REPO/images/agent/Containerfile" "$REPO"
fi

say "preparing $WORK"
rm -rf "$WORK"; mkdir -p "$WORK"
cp -r "$REPO/examples/quickstart/hub" "$WORK/hub"
sed -i "s#unix:///run/ocl/supervisor.sock#unix://$RUN_DIR/supervisor.sock#" "$WORK/hub/node.yaml"
printf 'link_socket: %s/supervisor.sock\n' "$RUN_DIR" >> "$WORK/hub/supervisor.yaml"

PIDS=()
cleanup() {
  [ "$KEEP" = 1 ] && { echo "left running (--keep): ${PIDS[*]}"; return; }
  say "stopping"
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  podman ps -a --filter label=ocl.session --format '{{.Names}}' | xargs -r podman rm -f >/dev/null
}
trap cleanup EXIT

# The provider key lives only in the node's environment; the mock rejects
# requests that do not carry it, which proves keyproxy injected it.
export MOCK_API_KEY="mock-provider-key-$RANDOM$RANDOM"
python3 "$REPO/examples/mock-llm/mock_llm.py" --port 8099 --require-key "$MOCK_API_KEY" > "$WORK/mock.log" 2>&1 &
PIDS+=($!)

"$BIN/ocl-supervisor" --config-dir "$WORK/hub" init > "$WORK/init.txt"
export OCL_TOKEN="$(cat "$WORK/hub/cli-token")"
"$BIN/ocl-supervisor" --config-dir "$WORK/hub" run > "$WORK/supervisor.log" 2>&1 &
PIDS+=($!)
sleep 1
"$BIN/opencode-node" --config-dir "$WORK/hub" run > "$WORK/node-hub.log" 2>&1 &
PIDS+=($!)

if [ "$WITH_WORKER" = 1 ]; then
  mkdir -p "$WORK/worker-a/agents"
  cp "$WORK/hub/providers.yaml" "$WORK/worker-a/"; cp -r "$WORK/hub/prompts" "$WORK/worker-a/"
  cp "$WORK/hub/agents/builder.yaml" "$WORK/worker-a/agents/"
  cat > "$WORK/worker-a/node.yaml" <<EOF
name: worker-a
supervisor: unix://$RUN_DIR/supervisor.sock
max_sessions: 4
runtime: { kind: podman, run_dir: $RUN_DIR-worker-a }
EOF
  "$BIN/opencode-node" --config-dir "$WORK/worker-a" run > "$WORK/node-worker-a.log" 2>&1 &
  PIDS+=($!)
fi
sleep 3

O="$BIN/ocl"
say "agents"
$O agents

say "demo 1: workflow build-review"
$O workflow run build-review "Write greet(name) in greet.py: strip whitespace, default to 'world'. Include pytest tests."

say "demo 2: urgent task for the coordinator"
$O task add "Later: tidy the README" --label when_free
$O task add "Release notes (manual)" --label on_demand
TASK=$($O task add "Greeting module" -b "Create greet(name) with tests and get it reviewed." --label urgent --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo "waiting for the coordinator's builder to start..."
B=""
for _ in $(seq 1 60); do
  sleep 3
  B=$($O sessions --json | python3 -c 'import sys,json;print(next((s["id"] for s in json.load(sys.stdin) if s["agent"]=="builder" and s["state"]=="running" and (s.get("title") or "").startswith("implement")),""))')
  [ -n "$B" ] && break
done
if [ -n "$B" ]; then
  say "operator sends a mid-run command to $B"
  $O send "$B" "Please also add a docstring to greet()."
fi

for _ in $(seq 1 80); do
  sleep 5
  st=$($O task show "$TASK" --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  run=$($O workflow runs | sed -n 1p)
  [ "$st" != running ] && [ "$st" != queued ] && ! grep -q Running <<<"$run" && break
done

say "results"
$O workflow runs | sed -n 1p
$O task show "$TASK" --json | python3 -c 'import sys,json;t=json.load(sys.stdin);print("task #%s: %s - %s" % (t["id"], t["status"], t["result"]))'
$O status
say "greet.py produced for the task (note the docstring from the mid-run command)"
cat "$WORK/hub/data/workspaces/shared-task-$TASK/greet.py" || true
say "audit chain"
$O audit --verify
echo
echo "Logs: $WORK/*.log    Token: $WORK/hub/cli-token    API: http://127.0.0.1:7700"
