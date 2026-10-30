#!/usr/bin/env bash
# Messages for a paused session wait at the hub, in order, and reach it in one
# message marked as past events when it resumes. Nothing is lost.
source "$(dirname "$0")/lib.sh"

say "hub with room for two sessions"
start_hub
kill "${PIDS[2]}"; sleep 1
sed -i 's/^max_sessions: 6$/max_sessions: 2/' "$W/hub/node.yaml"
"$BIN/moocher-node" --config-dir "$W/hub" run >> "$W/node-hub.log" 2>&1 &
PIDS[2]=$!

mock_script slowk <<'JSON'
[{"text": "CHILD-RESULT-42", "delay": 70}]
JSON
mock_script hcoord <<'JSON'
{"phases": [
  {"match": "RUSH", "steps": [
    {"tool": "cluster_task_complete", "args": {"summary": "rushed"}, "delay": 50},
    {"text": "done"}
  ]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "slowk", "prompt": "work"}}, {"text": "asked"}]}
]}
JSON
mock_prices slowk hcoord
agent_yaml "$W/hub" slowk slowk
agent_yaml "$W/hub" coordinator hcoord
wait_for 30 "agents loaded" bash -c "$O agents | grep -q slowk"
tstat() { $O task ls --json | python3 -c "import sys,json;print([t['status'] for t in json.load(sys.stdin) if t['id']==$1][0])"; }
tadd() { $O task add "$@" --json | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])"; }
of() { $O sessions --json | python3 -c "import sys,json;print(' '.join(s['id'] for s in json.load(sys.stdin) if s['agent']=='$1'))"; }
held() { sess 's.get("held",0)' "$1"; }
export -f tstat of held

say "the manager is off until configured"
expect "it says how to turn it on" bash -c "curl -s -H 'Authorization: Bearer $MOO_TOKEN' http://127.0.0.1:7700/api/v1/manager | grep -q 'set manager.agent'"
expect_fail "and refuses to answer" $O ask "anything"

say "a coordinator waits for its worker, and is paused"
T1=$(tadd "first" --label normal)
wait_for 90 "the worker runs" bash -c "[ -n \"\$(of slowk)\" ] && [ \"\$(sess_state \$(of slowk))\" = running ]"
C=$(of coordinator)
K=$(of slowk)
wait_for 60 "the coordinator is idle" bash -c "[ \"\$(sess_state $C)\" = idle ]"
TR=$(tadd "RUSH this" --label right_now)
wait_for 30 "right_now pauses the idle coordinator" bash -c "[ \"\$(sess_state $C)\" = halted ]"
expect "the worker keeps running" bash -c "[ \"\$(sess_state $K)\" = running ]"

say "what arrives meanwhile is held"
expect "a message from the operator is accepted" $O send "$C" "OPERATOR-NOTE-7"
expect "and held" bash -c "[ \"\$(held $C)\" = 1 ]"
wait_for 120 "the worker's result is held too, not dropped" bash -c "[ \"\$(held $C)\" = 2 ]"
expect "the event log says so" bash -c "$O events --session $C --limit 200 | grep -q message.held"

say "resuming delivers it all, once, marked as past"
wait_for 120 "the right_now task finishes" bash -c "[ \"\$(tstat $TR)\" = done ]"
wait_for 120 "it reaches the model, with the note that it resumed" bash -c "curl -s http://127.0.0.1:8099/v1/_log | python3 -c \"import sys;s=sys.stdin.read();assert 'PAST events' in s and 'have resumed' in s\""
expect "nothing is held any more" bash -c "[ \"\$(held $C)\" = 0 ]"
expect "the event log says so" bash -c "$O events --session $C --limit 200 | grep -q message.delivered"
curl -s http://127.0.0.1:8099/v1/_log > "$W/log.json"
python3 -I - "$W/log.json" > "$W/digest.txt" <<'PY'
import json, sys
seen = []
for e in json.load(open(sys.argv[1])):
    for m in e.get("messages", []):
        c = m.get("content")
        c = c if isinstance(c, str) else " ".join(p.get("text", "") for p in c or [] if isinstance(p, dict))
        if m.get("role") == "user" and "PAST events" in c and c not in seen:
            seen.append(c)
print(len(seen))
print("\n".join(seen))
PY
expect "it arrived as one message" bash -c "[ \"\$(head -1 $W/digest.txt)\" = 1 ]"
expect "with both, in the order they came" python3 -c "s=open('$W/digest.txt').read();assert -1<s.index('OPERATOR-NOTE-7')<s.index('CHILD-RESULT-42');assert '2 messages arrived' in s"
stop_all

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
