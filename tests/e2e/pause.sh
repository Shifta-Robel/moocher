#!/usr/bin/env bash
# Pausing a task: agents stop at their next model call with everything kept, give
# up their slots, and carry on after resume. --now cuts them off, and so does the
# grace period. A paused task holds nothing back and keeps its questions.
source "$(dirname "$0")/lib.sh"

say "hub"
TASKS_YAML="pause_grace_secs: 12" start_hub
inbox='{"tool": "cluster_read_inbox", "args": {}}'
coord() { # coord NAME WORKER
  mock_script "$1" <<JSON
{"phases": [
  {"match": "has resumed", "steps": [{"text": "$1 back"}]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "$2", "prompt": "work"}}, {"text": "asked"}]}
]}
JSON
}
mock_script gentlem <<JSON
{"phases": [
  {"match": "nothing was cut off", "steps": [{"text": "gentle finished"}]},
  {"steps": [{"tool": "cluster_read_inbox", "args": {}, "delay": 10}, $inbox, {"text": "should have been paused"}]}
]}
JSON
mock_script hardm <<JSON
{"phases": [
  {"match": "was cut off", "steps": [{"text": "hard finished"}]},
  {"steps": [{"text": "too late", "delay": 120}]}
]}
JSON
mock_script graceem <<JSON
{"phases": [
  {"match": "was cut off", "steps": [{"text": "grace finished"}]},
  {"steps": [{"text": "too late", "delay": 120}]}
]}
JSON
mock_script askm <<'JSON'
{"phases": [
  {"match": "has resumed", "steps": [{"text": "ask back"}]},
  {"steps": [{"tool": "cluster_ask", "args": {"question": "PAUSE-QUESTION: which one?", "importance": "vital"}}, {"text": "asked"}]}
]}
JSON
mock_script plainm <<'JSON'
[{"text": "plain here"}]
JSON
for c in "cg gentle" "ch hard" "cl gracee" "ca asker"; do set -- $c; coord "$1" "$2"; done
mock_prices gentlem hardm graceem askm plainm cg ch cl ca
agent_yaml "$W/hub" gentle gentlem
agent_yaml "$W/hub" hard hardm
agent_yaml "$W/hub" gracee graceem
agent_yaml "$W/hub" asker askm
agent_yaml "$W/hub" plain plainm
wait_for 30 "agents loaded" bash -c "$O agents | grep -q plain"
tadd() { $O task add "$@" --json | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])"; }
tstat() { $O task ls --json | python3 -c "import sys,json;print([t['status'] for t in json.load(sys.stdin) if t['id']==$1][0])"; }
of() { $O sessions --json | python3 -c "import sys,json;print(*[s['id'] for s in json.load(sys.stdin) if s['agent']=='$1'][-1:])"; }
calls() { curl -s http://127.0.0.1:8099/v1/_log | python3 -c "import sys,json;print(sum(e['model']=='$1' for e in json.load(sys.stdin)))"; }
set_coordinator() { agent_yaml "$W/hub" coordinator "$1"; sleep 3; }
export -f tstat of calls

say "a task that has not started goes back to the queue"
TQ=$(tadd "later" --label on_demand)
$O task pause "$TQ" >/dev/null
expect "it is paused" bash -c "[ \"\$(tstat $TQ)\" = paused ]"
$O task resume "$TQ" >/dev/null
expect "resume queues it again" bash -c "[ \"\$(tstat $TQ)\" = queued ]"
$O task rm "$TQ" >/dev/null

say "by default agents stop at their next model call"
set_coordinator cg
TG=$(tadd "gentle" --label urgent)
wait_for 150 "the worker is in a model call" bash -c "[ -n \"\$(of gentle)\" ] && [ \"\$(calls gentlem)\" -ge 1 ]"
W1=$(of gentle)
$O task pause "$TG" >/dev/null
expect "the task is pausing" bash -c "[ \"\$(tstat $TG)\" = pausing ]"
wait_for 150 "then paused" bash -c "[ \"\$(tstat $TG)\" = paused ]"
expect "the worker is halted" bash -c "[ \"\$(sess_state $W1)\" = halted ]"
expect "it was not cut off" bash -c "[ \"\$(sess 's[\"task_paused\"][\"cut_off\"]' $W1)\" = False ]"
N=$(calls gentlem)
expect "the call in progress finished, the next never went out" bash -c "[ $N = 1 ]"
expect "the workspace and slots are free" bash -c "[ \"\$($O nodes --json | python3 -c 'import sys,json;print(json.load(sys.stdin)[0][\"running\"])')\" = 0 ]"
$O send "$W1" "MESSAGE-WHILE-PAUSED" >/dev/null 2>&1
expect "a message waits at the hub" bash -c "[ \"\$(sess 's[\"held\"]' $W1)\" = 1 ]"

say "a paused task holds nothing back"
TP=$(tadd "plain" --label normal)
set_coordinator plainm
wait_for 150 "other work starts" bash -c "[ \"\$(tstat $TP)\" = running ]"
$O task cancel "$TP" >/dev/null

say "the node restarts during the pause, and the task resumes"
kill "${PIDS[2]}"; sleep 2
"$BIN/moocher-node" --config-dir "$W/hub" run >> "$W/node-hub.log" 2>&1 &
PIDS[2]=$!
wait_for 30 "the node is back" bash -c "$O nodes | grep -q connected"
$O task resume "$TG" >/dev/null
wait_for 60 "the task runs again" bash -c "[ \"\$(tstat $TG)\" = running ]"
wait_for 150 "the worker carries on where it was" bash -c "[ \"\$(sess 's[\"last_text\"]' $W1)\" = 'gentle finished' ]"
wait_for 20 "and got what was sent to it" bash -c "curl -s http://127.0.0.1:8099/v1/_log | grep -q MESSAGE-WHILE-PAUSED"
$O task cancel "$TG" >/dev/null

say "--now cuts them off at once, the coordinator too"
set_coordinator ch
TH=$(tadd "hard" --label urgent)
wait_for 150 "the worker is in a long call" bash -c "[ -n \"\$(of hard)\" ] && [ \"\$(calls hardm)\" -ge 1 ]"
W2=$(of hard)
$O task pause "$TH" --now >/dev/null
wait_for 30 "paused right away" bash -c "[ \"\$(tstat $TH)\" = paused ]"
expect "the worker knows it was cut off" bash -c "[ \"\$(sess 's[\"task_paused\"][\"cut_off\"]' $W2)\" = True ]"
$O task resume "$TH" >/dev/null
wait_for 150 "it resumes and is told" bash -c "[ \"\$(sess 's[\"last_text\"]' $W2)\" = 'hard finished' ]"
$O task cancel "$TH" >/dev/null

say "an agent that does not reach a clean point is cut off after the grace period"
set_coordinator cl
TL=$(tadd "grace" --label urgent)
wait_for 150 "the worker is in a long call" bash -c "[ -n \"\$(of gracee)\" ] && [ \"\$(calls graceem)\" -ge 1 ]"
W3=$(of gracee)
$O task pause "$TL" >/dev/null
sleep 5
expect "it is still pausing within the grace period" bash -c "[ \"\$(tstat $TL)\" = pausing ]"
wait_for 30 "then paused" bash -c "[ \"\$(tstat $TL)\" = paused ]"
expect "and marked as cut off" bash -c "[ \"\$(sess 's[\"task_paused\"][\"cut_off\"]' $W3)\" = True ]"
$O task cancel "$TL" >/dev/null
wait_for 20 "a paused task can be cancelled" bash -c "[ \"\$(tstat $TL)\" = cancelled ]"
wait_for 20 "which ends its agents" bash -c "[ \"\$(sess_state $W3)\" = stopped ]"

say "an agent waiting for an answer keeps its question"
set_coordinator ca
TA=$(tadd "ask" --label urgent)
qopen() { $O questions --json | grep -q PAUSE-QUESTION; }
export -f qopen
wait_for 150 "the question is open" qopen
$O task pause "$TA" >/dev/null
wait_for 30 "paused" bash -c "[ \"\$(tstat $TA)\" = paused ]"
sleep 4
expect "the question is still open" qopen
$O task cancel "$TA" >/dev/null
stop_all
[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
