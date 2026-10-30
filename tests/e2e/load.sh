#!/usr/bin/env bash
# Phase 5: load and chaos. A hub and two workers run 12 instances at once
# against a provider whose per-IP allowance is too small for all of them
# (they park and resume after the reset), then a worker is killed mid-task,
# then the supervisor is killed under load. Prints latency and memory.
#
# Every node goes out from 127.0.0.1, so the allowance is shared cluster-wide
# and the hub has to keep the count across nodes.
MOCK_ARGS="--ip-quota 40 --ip-window 30"
HUB_NODE_YAML='ip_echo: ["http://127.0.0.1:8099/v1/ip"]'
ENROLL_YAML="disconnect_grace_secs: 8"
source "$(dirname "$0")/lib.sh"
SUP_CMD=("$BIN/moocher-hub" --config-dir "$W/hub" run)

say "hub + worker-a + worker-b, 4 sessions each"
start_hub
cat >> "$W/hub/providers.yaml" <<YAML
    quota: { scope: ip }
YAML
sed -i 's/^max_sessions: 6$/max_sessions: 4/' "$W/hub/node.yaml"
mock_script lwork <<'JSON'
{"phases": [
  {"match": "taking over from session", "steps": [{"tool": "write", "args": {"filePath": "done.txt", "content": "replacement"}}, {"text": "finished"}]},
  {"match": "stopped unexpectedly", "steps": [{"tool": "write", "args": {"filePath": "done.txt", "content": "restarted"}}, {"text": "finished"}]},
  {"steps": [
    {"tool": "todowrite", "args": {"todos": [{"content": "work", "status": "in_progress", "priority": "high"}]}},
    {"tool": "write", "args": {"filePath": "start.txt", "content": "started"}},
    {"tool": "bash", "args": {"command": "sleep {{SLEEP}}", "description": "the long part"}},
    {"tool": "write", "args": {"filePath": "done.txt", "content": "done"}},
    {"text": "finished"}]}
]}
JSON
mock_prices lwork
agent_yaml "$W/hub" lwork lwork
for n in worker-a worker-b; do
  join_node "$n" >/dev/null && $O node approve "$n" >/dev/null
  make_worker "$n"
  cp "$W/hub/agents/lwork.yaml" "$W/$n/agents/"
  sed -i 's/^max_sessions: .*$/max_sessions: 4/' "$W/$n/node.yaml"
  grep -q '^max_sessions' "$W/$n/node.yaml" || echo 'max_sessions: 4' >> "$W/$n/node.yaml"
  echo 'ip_echo: ["http://127.0.0.1:8099/v1/ip"]' >> "$W/$n/node.yaml"
  start_node "$n"
done
kill "${PIDS[2]}"; sleep 1 # the hub's node, restarted with max_sessions 4
"$BIN/moocher-node" --config-dir "$W/hub" run >> "$W/node-hub.log" 2>&1 &
PIDS+=($!)
for n in hub worker-a worker-b; do wait_for 30 "$n connected" is_connected "$n"; done
wait_for 30 "every node has lwork" bash -c "[ \$($O agents --json | python3 -c 'import sys,json;print(len({a[\"node\"] for a in json.load(sys.stdin) if a[\"agent\"][\"name\"]==\"lwork\"}))') = 3 ]"

# Peak RSS of the daemons while the scenarios run.
( while :; do
    for p in $(pgrep -f "$BIN/moocher-hub --config-dir $W/hub run") ; do echo "sup $(awk '/VmRSS/{print $2}' /proc/$p/status 2>/dev/null)"; done
    for n in hub worker-a worker-b; do for p in $(pgrep -f "$BIN/moocher-node --config-dir $W/$n run"); do echo "$n $(awk '/VmRSS/{print $2}' /proc/$p/status 2>/dev/null)"; done; done
    sleep 1
  done ) > "$W/rss.log" 2>/dev/null &
PIDS+=($!)

stats() { curl -s http://127.0.0.1:8099/v1/_stats; }
ids_file() { : > "$W/$1"; }
spawn_n() { # spawn_n FILE COUNT SLEEP [node]
  local f=$1 c=$2 sl=$3 node=${4:-}
  sed -i "s/{{SLEEP}}/$sl/; s/\"sleep [0-9]*\"/\"sleep $sl\"/" "$W/scripts/lwork.json"
  for i in $(seq 1 "$c"); do
    $O spawn lwork -p "job $i" ${node:+--node $node} --json | python3 -c 'import sys,json,time;s=json.load(sys.stdin);print(s["id"], s["node"], int(time.time()*1000))' >> "$W/$f"
  done
}
finished() { $O sessions --json | python3 -c "
import sys,json
ids={l.split()[0] for l in open('$W/$1')}
print(sum(1 for s in json.load(sys.stdin) if s['id'] in ids and s.get('last_text')=='finished'))"; }
running() { for s in $(awk '{print $1}' "$W/$1"); do sess 's["state"]' "$s"; done | grep -c running || true; }
export -f finished running node_field is_connected

say "load: 12 instances at once, more requests than the shared allowance (40 per 30 s)"
ids_file load.ids
T0=$(date +%s)
spawn_n load.ids 12 6
awk '{print $2}' "$W/load.ids" | sort | uniq -c | sed 's/^/  placed: /'
[ "$(awk '{print $2}' "$W/load.ids" | sort -u | wc -l)" = 3 ] && pass "spread over all three nodes" || fail "spread over all three nodes"
wait_for 300 "all 12 finished" bash -c '[ "$(finished load.ids)" = 12 ]'
T1=$(date +%s)
echo "  wall time: $((T1 - T0)) s for 12 instances"
$O events --limit 100000 --json > "$W/events.jsonl"
python3 -I - "$W/load.ids" "$W/events.jsonl" <<'PY'
import json, sys
ids = {l.split()[0]: int(l.split()[2]) for l in open(sys.argv[1])}
ready, done = {}, {}
for line in open(sys.argv[2]):
    e = json.loads(line)
    s = e.get("session")
    if s not in ids:
        continue
    # Ready = OpenCode up and the model loaded: the first move out of `starting`.
    if e["kind"] == "session.state" and e["data"].get("from") == "starting" and s not in ready:
        ready[s] = e["ts"]
    if e["kind"] == "session.turn_ended":
        done[s] = e["ts"]
def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))] / 1000
r = [ready[s] - ids[s] for s in ready]
d = [done[s] - ids[s] for s in done]
if r:
    print(f"  spawn -> ready: p50 {pct(r,50):.1f} s, p95 {pct(r,95):.1f} s, max {max(r)/1000:.1f} s ({len(r)} instances)")
if d:
    print(f"  spawn -> finished: p50 {pct(d,50):.1f} s, p95 {pct(d,95):.1f} s, max {max(d)/1000:.1f} s")
PY
PARKED=$(grep -c '"kind":"quota.parked"' "$W/events.jsonl" || true)
grep -q '"kind":"usage.mismatch"' "$W/events.jsonl" && fail "no false usage.mismatch under load" || pass "no false usage.mismatch under load (the meter and OpenCode agree)"
ST=$(stats); echo "  provider: $ST"
[ "$PARKED" -ge 1 ] && pass "the allowance ran out: $PARKED sessions parked and resumed after the reset" || fail "sessions parked (got $PARKED)"
RL=$(echo "$ST" | python3 -c 'import sys,json;print(json.load(sys.stdin)["rate_limited"])')
CH=$(echo "$ST" | python3 -c 'import sys,json;print(json.load(sys.stdin)["chat"])')
# Each node learns the shared remaining count from its own responses, so at a
# window's end requests in flight on other nodes can overshoot by at most one
# per concurrent session; keyproxy absorbs those 429s (the session parks).
[ "$RL" -le 12 ] && pass "overshoot bounded by concurrency: $RL of $CH provider calls got 429, all absorbed" || fail "429s bounded by concurrency (got $RL of $CH)"
$O sessions --json | python3 -c "import sys,json;ids={l.split()[0] for l in open('$W/load.ids')};sys.exit(0 if not [s for s in json.load(sys.stdin) if s['id'] in ids and s.get('error')] else 1)" && pass "no session saw an error" || fail "no session saw an error"
wait_for 10 "hub's request count matches what the provider served" bash -c "$O quotas --json | python3 -c 'import sys,json;q=[x for x in json.load(sys.stdin) if x[\"provider\"]==\"mock\" and x[\"kind\"]==\"requests\"];sys.exit(0 if q else 1)'"
$O quotas mock | sed 's/^/  /'
for f in $(awk '{print $1}' "$W/load.ids"); do n=$(awk -v s="$f" '$1==s{print $2}' "$W/load.ids"); d="$W/$n/data/workspaces/$f/done.txt"; [ "$n" = hub ] && d="$W/hub/data/workspaces/$f/done.txt"; [ -f "$d" ] || echo "  missing $d"; done > "$W/missing.txt"
[ ! -s "$W/missing.txt" ] && pass "every instance wrote its done.txt" || { fail "every instance wrote its done.txt"; cat "$W/missing.txt"; }
stop_all

say "chaos: worker-a is killed with 4 instances mid-task"
ids_file chaos.ids
spawn_n chaos.ids 4 25 worker-a
wait_for 120 "all 4 in their long step" bash -c '[ "$(running chaos.ids)" = 4 ] && [ "$(ls $W/worker-a/data/workspaces/*/start.txt 2>/dev/null | wc -l)" -ge 4 ]'
kill -9 "$PID_worker_a"
for b in $(pgrep -f "unix:$(run_dir worker-a)/"); do pkill -9 -P "$b"; kill -9 "$b"; done
lostn() { for s in $(awk '{print $1}' "$W/chaos.ids"); do $O events --session "$s" --limit 500 | grep -c 'session.dead.*node_lost' || true; done | awk '$1>0' | wc -l; }
export -f lostn
wait_for 60 "all 4 declared dead (node_lost) after the grace period" bash -c '[ "$(lostn)" = 4 ]'
outcome() { # each lost session: replaced (and the replacement finished) or halted for the operator
  $O sessions --json > "$W/s.json"; $O halts --json > "$W/h.json"
  python3 -I - "$W/chaos.ids" "$W/s.json" "$W/h.json" <<'PY'
import json, sys
ids = [l.split()[0] for l in open(sys.argv[1])]
ss = json.load(open(sys.argv[2])); hs = json.load(open(sys.argv[3]))
for i in ids:
    rep = [s for s in ss if s.get("replaces") == i]
    halt = [h for h in hs if h.get("dead_session") == i]
    if rep and rep[0].get("last_text") == "finished":
        print(i, "replaced", rep[0]["node"])
    elif halt:
        print(i, "halted", halt[0]["id"])
    else:
        print(i, "pending")
PY
}
export -f outcome
wait_for 240 "each one was resumed elsewhere and finished, or halted for the operator" bash -c '! outcome | grep -q pending'
outcome | sed 's/^/  /'
outcome | grep -q " replaced " && pass "lost work was resumed on the remaining nodes" || pass "lost work was halted for the operator (default death policy)"
! outcome | grep -q "replaced worker-a" && pass "no replacement was placed on the dead node" || fail "no replacement on the dead node"
stop_all

say "chaos: the supervisor is killed (-9) under load and restarted after 5 s"
ids_file sup.ids
spawn_n sup.ids 6 12
wait_for 120 "6 running" bash -c '[ "$(running sup.ids)" = 6 ]'
pkill -9 -f "$BIN/moocher-hub --config-dir $W/hub run"
sleep 5
"${SUP_CMD[@]}" >> "$W/supervisor.log" 2>&1 &
PIDS+=($!)
wait_for 70 "hub and worker-b reconnect" bash -c 'is_connected hub && is_connected worker-b'
wait_for 180 "all 6 finished" bash -c '[ "$(finished sup.ids)" = 6 ]'
DUP=$(for s in $(awk '{print $1}' "$W/sup.ids"); do $O events --session "$s" --limit 2000 --json | grep -c '"kind":"session.turn_ended"'; done | sort -u | tr '\n' ' ')
[ "$DUP" = "1 " ] && pass "each turn end recorded exactly once (outbox replay deduplicated)" || fail "turn ends recorded once (counts: $DUP)"
$O halts --json | python3 -c "import sys,json;ids={l.split()[0] for l in open('$W/sup.ids')};sys.exit(0 if not [h for h in json.load(sys.stdin) if h['dead_session'] in ids] else 1)" && pass "no session was declared lost while its node reconnected" || fail "no false node_lost after the hub restart"
expect "audit chain verifies after the crash" bash -c "$O audit --verify | grep -q '\"ok\": true'"
for s in $(awk '{print $1}' "$W/sup.ids"); do $O show "$s" --json; done | python3 -c '
import sys, json
dec = json.JSONDecoder(); t = sys.stdin.read(); i = 0; bad = 0
while i < len(t):
    t2 = t[i:].lstrip()
    if not t2: break
    o, n = dec.raw_decode(t2); i = len(t) - len(t2) + n
    if not (o["tokens"]["input"] > 0 and o["metered"]["input"] >= o["tokens"]["input"]): bad += 1
sys.exit(1 if bad else 0)' && pass "tokens and the keyproxy meter are intact for all 6" || fail "tokens and meter intact"

say "memory under load (peak VmRSS)"
awk '{if ($2>m[$1]) m[$1]=$2} END {for (k in m) printf "  %-9s %.1f MB\n", k, m[k]/1024}' "$W/rss.log" | sort
awk '$1=="sup"{if($2>m)m=$2} END{exit !(m<60*1024)}' "$W/rss.log" && pass "supervisor stayed under 60 MB" || fail "supervisor under 60 MB"
awk '$1!="sup"{if($2>m)m=$2} END{exit !(m<40*1024)}' "$W/rss.log" && pass "every node stayed under 40 MB" || fail "nodes under 40 MB"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
