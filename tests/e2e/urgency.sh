#!/usr/bin/env bash
# Generic agents (just a model; the spawner gives the specialization), urgency
# (normal respects the task cap, urgent overtakes in the spawn queue, right_now
# pauses less urgent work and it resumes afterwards) and costly models kept for
# urgent work.
source "$(dirname "$0")/lib.sh"

say "hub with room for three sessions"
start_hub
kill "${PIDS[2]}"; sleep 1
sed -i 's/^max_sessions: 6$/max_sessions: 3/' "$W/hub/node.yaml"
"$BIN/moocher-node" --config-dir "$W/hub" run >> "$W/node-hub.log" 2>&1 &
PIDS[2]=$!

mock_script genm <<'JSON'
[{"text": "generic here"}]
JSON
mock_script plainm <<'JSON'
[{"text": "plain here"}]
JSON
mock_script premm <<'JSON'
[{"text": "premium here"}]
JSON
mock_script solom <<'JSON'
[{"text": "solo here"}]
JSON
mock_script gcoord <<'JSON'
{"phases": [
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "gen", "prompt": "hi", "system_prompt": "You are NINJA-7."}}, {"text": "asked"}]}
]}
JSON
mock_script qcoord <<'JSON'
{"phases": [
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "solo", "prompt": "hi", "if_busy": "wait", "wait_max_s": 300}}, {"text": "asked"}]}
]}
JSON
mock_script pcoord <<'JSON'
{"phases": [
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "prem", "prompt": "hi"}}, {"text": "asked"}]}
]}
JSON
mock_script tcoord <<'JSON'
{"phases": [
  {"match": "RUSH", "steps": [
    {"tool": "cluster_task_complete", "args": {"summary": "rushed"}, "delay": 8},
    {"text": "done"}
  ]},
  {"match": "paused", "steps": [{"text": "resumed fine"}]},
  {"steps": [{"text": "long job", "delay": 240}]}
]}
JSON
mock_prices genm plainm premm solom gcoord qcoord pcoord tcoord
cat > "$W/hub/agents/gen.yaml" <<'YAML'
name: gen
kind: generic
provider: { id: mock, model: genm, key: { ref: "env:MOCK_API_KEY" } }
YAML
agent_yaml "$W/hub" plain plainm
agent_yaml "$W/hub" gcoord gcoord
agent_yaml "$W/hub" qcoord qcoord
agent_yaml "$W/hub" solo solom "instances: { max: 1 }"
agent_yaml "$W/hub" prem premm "min_urgency: right_now"
agent_yaml "$W/hub" pcoord pcoord
agent_yaml "$W/hub" coordinator tcoord
wait_for 30 "agents loaded" bash -c "$O agents | grep -q pcoord"
role() { cat "$W/hub/data/instances/$1/etc/ROLE.md"; }
avail() { curl -s -H "Authorization: Bearer $MOO_TOKEN" http://127.0.0.1:7700/api/v1/agents/availability | python3 -c "import sys,json;a=[x for x in json.load(sys.stdin) if x['agent']=='$1'];print(a[0]['$2'] if a else '')"; }
child_of() { $O sessions --json | python3 -c "import sys,json;print(' '.join(s['id'] for s in json.load(sys.stdin) if s.get('parent')=='$1' and s['state'] in ('idle','running','starting')))"; }
live_of() { $O sessions --json | python3 -c "import sys,json;print(' '.join(s['id'] for s in json.load(sys.stdin) if s['agent']=='$1' and s['state'] in ('idle','running','starting')))"; }
tstat() { $O task ls --json | python3 -c "import sys,json;print([t['status'] for t in json.load(sys.stdin) if t['id']==$1][0])"; }
tadd() { $O task add "$@" --json | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])"; }
export -f role avail child_of live_of tstat

say "a generic agent is just a model; the spawner specializes it"
expect "list_agents says it is generic" bash -c "[ \"\$(avail gen generic)\" = True ]"
expect_fail "it will not start without a system prompt" $O spawn gen -p "hi"
expect_fail "a specialized agent refuses one" $O spawn plain -p "hi" --system-prompt "be someone else"
G=$(spawn_id gen -p "hi" --system-prompt "You are PIRATE-9.")
wait_for 60 "it answers" bash -c "[ \"\$(sess 's[\"last_text\"]' $G)\" = 'generic here' ]"
expect "its instructions have the spawner's prompt" bash -c "role $G | grep -q 'You are PIRATE-9.'"
expect "and still say what it is" bash -c "role $G | grep -q '# Role: gen'"
stop_all
P=$(spawn_id gcoord --coordinator -p "start")
wait_for 60 "a coordinator starts one with its own prompt" bash -c "[ -n \"\$(child_of $P)\" ]"
C=$(child_of "$P")
expect "the child got that prompt" bash -c "role $C | grep -q 'You are NINJA-7.'"
stop_all

say "costly models are kept for urgent work"
expect "operators can always start one" bash -c "$O spawn prem -p hi --json >/dev/null"
stop_all
PN=$(spawn_id pcoord --coordinator -p "start")
wait_for 60 "a normal coordinator finishes its turn" bash -c "[ \"\$(sess 's[\"last_text\"]' $PN)\" = 'asked' ]"
expect "its spawn of the costly model was refused" bash -c "[ -z \"\$(child_of $PN)\" ]"
stop_all
PR=$(spawn_id pcoord --coordinator -p "start" --urgency right_now)
wait_for 60 "a right_now coordinator gets it" bash -c "[ -n \"\$(child_of $PR)\" ]"
stop_all

say "urgent goes ahead of normal in the spawn queue"
S=$(spawn_id solo -p "hi")
wait_for 60 "solo holds its one instance" bash -c "[ \"\$(sess_state $S)\" = idle ]"
QN=$(spawn_id qcoord --coordinator -p "start" --urgency normal)
wait_for 60 "a normal request queues" bash -c "[ \"\$(sess 's[\"last_text\"]' $QN)\" = asked ]"
QU=$(spawn_id qcoord --coordinator -p "start" --urgency urgent)
wait_for 60 "a later urgent request queues" bash -c "[ \"\$(sess 's[\"last_text\"]' $QU)\" = asked ]"
$O stop "$S" >/dev/null
wait_for 60 "the urgent request starts first" bash -c "[ -n \"\$(child_of $QU)\" ]"
expect "the normal one is still waiting" bash -c "[ -z \"\$(child_of $QN)\" ]"
stop_all
wait_for 30 "the queue drains" bash -c "[ -z \"\$(live_of solo)\" ]"
stop_all

say "right_now pauses less urgent work, and it resumes"
H=$(spawn_id plain -p "mine, by hand")
wait_for 60 "a session started by hand" bash -c "[ \"\$(sess_state $H)\" = idle ]"
T1=$(tadd "first" --label normal)
T2=$(tadd "second" --label normal)
T3=$(tadd "third" --label normal)
wait_for 30 "normal tasks start up to the cap" bash -c "[ \"\$(tstat $T1)\" = running ] && [ \"\$(tstat $T2)\" = running ]"
expect "the third waits" bash -c "[ \"\$(tstat $T3)\" = queued ]"
$O task cancel "$T3" >/dev/null 2>&1
V=$($O sessions --json | python3 -c "import sys,json;print(max((s for s in json.load(sys.stdin) if s['agent']=='coordinator' and s['state'] in ('idle','running','starting')),key=lambda s:s['started_at'])['id'])")
wait_for 90 "both are past booting (a session still starting cannot be paused)" bash -c "[ \"\$(sess_state $V)\" = running ]"
D=$(sess 's["directory"]' "$V")
echo keep-me > "$D/marker.txt"
TR=$(tadd "RUSH this" --label right_now)
wait_for 30 "the right_now task starts at once" bash -c "[ \"\$(tstat $TR)\" != queued ]"
wait_for 30 "the newest normal session is paused" bash -c "[ \"\$(sess_state $V)\" = halted ]"
expect "the session started by hand is left alone" bash -c "[ \"\$(sess_state $H)\" = idle ]"
expect "the event names who it made way for" bash -c "$O events --session $V --limit 100 | grep -q 'session.preempted.*task #$TR'"
expect "a notification says so" bash -c "$O notifications --json | grep -q 'paused for task #$TR'"
wait_for 90 "the right_now task finishes" bash -c "[ \"\$(tstat $TR)\" = done ]"
wait_for 60 "the paused session resumes" bash -c "[ \"\$(sess_state $V)\" != halted ] && [ \"\$(sess_state $V)\" != stopped ]"
expect "the event says so" bash -c "$O events --session $V --limit 100 | grep -q session.resumed"
expect "its workspace is intact" grep -q keep-me "$D/marker.txt"
wait_for 60 "it was told why" bash -c "[ \"\$(sess 's[\"last_text\"]' $V)\" = 'resumed fine' ]"
stop_all

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
