#!/usr/bin/env bash
# What the cluster learns from provider calls, and what agents see before
# they spawn: per-model speed and error counts, a context window learned from
# an overflow error (and used for new sessions), credits running out (event,
# notification, allowance held back), agent availability (ready / busy with
# idle sessions) and a spawn that waits in the queue until there is room.
source "$(dirname "$0")/lib.sh"

say "hub with scripted failures"
export MOO_OBS_INTERVAL_S=2
start_hub
mock_script ovf <<'JSON'
[
  {"error": {"status": 400, "body": {"error": {"message": "This model's maximum context length is 32768 tokens. However, you requested 40000 tokens (36000 in the messages, 4000 in the completion).", "code": "context_length_exceeded"}}}, "times": 1},
  {"text": "fits now"}
]
JSON
mock_script broke <<'JSON'
[
  {"error": {"status": 402, "body": {"error": {"message": "Insufficient credits. Add more at https://example.org/credits"}}}, "times": 1},
  {"text": "paid"}
]
JSON
mock_script solo <<'JSON'
[{"text": "solo here"}]
JSON
mock_script qcoord <<'JSON'
{"phases": [
  {"match": "has started", "steps": [{"text": "dequeued"}]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "solo", "prompt": "hi", "if_busy": "wait", "wait_max_s": 300}}, {"text": "asked"}]}
]}
JSON
mock_prices ovf broke solo qcoord
cat > "$W/hub/agents/ovf.yaml" <<'EOF'
name: ovf
description: claims a bigger window than its model has
provider: { id: mock, model: ovf, key: { ref: "env:MOCK_API_KEY" }, context: 200000 }
tools: { edit: allow }
limits: { max_sessions: 4 }
EOF
agent_yaml "$W/hub" broke broke
agent_yaml "$W/hub" solo solo "instances: { max: 1 }"
agent_yaml "$W/hub" qcoord qcoord
wait_for 20 "agents loaded" bash -c "$O agents | grep -q qcoord"
stats() { $O providers --json | python3 -c "import sys,json;m=[x for x in json.load(sys.stdin)['models'] if x['model']=='$1'];print($2 if m else '')"; }
avail() { curl -s -H "Authorization: Bearer $MOO_TOKEN" http://127.0.0.1:7700/api/v1/agents/availability; }

agent_status() { avail | python3 -c "import sys,json;a=[x for x in json.load(sys.stdin) if x['agent']=='$1'];print(a[0]['$2'] if a else '')"; }
export -f stats avail agent_status

say "a context overflow teaches the model's real window"
O1=$(spawn_id ovf -p "go")
wait_for 90 "the window the provider named (32768) is learned" bash -c "[ \"\$(stats ovf 'm[0][\"learned\"][\"context\"]')\" = 32768 ]"
expect "the overflow is counted as an error of that class" bash -c "[ \"\$(stats ovf 'm[0][\"errors\"].get(\"context_overflow\",0)')\" -ge 1 ]"
expect "a notification says the template claims more" bash -c "$O notifications --json | grep -q 'has a 32768-token window'"
expect "moo providers --events lists it" bash -c "$O providers --events | grep -q 'context_overflow.*mock/ovf.*window 32768'"
O2=$(spawn_id ovf -p "again")
wait_for 30 "a new session of the same model" bash -c "[ -f $W/hub/data/instances/$O2/etc/opencode.json ]"
expect "is configured with the learned window" bash -c "python3 -c \"import json;c=json.load(open('$W/hub/data/instances/$O2/etc/opencode.json'));m=list(c['provider'].values())[0]['models']['ovf'];assert m['limit']['context']==32768, m\""
expect "agents see it in list_agents" bash -c "[ \"\$(agent_status ovf context)\" = 32768 ]"

say "speed and availability"
S1=$(spawn_id solo -p "hello")
wait_for 60 "solo answered" bash -c "[ \"\$(sess 's[\"last_text\"]' $S1)\" = 'solo here' ]"
wait_for 30 "calls to mock/solo are counted with a first-byte time" bash -c "[ -n \"\$(stats solo 'm[0][\"ttft_ms_p50\"] or \"\"')\" ] && [ \"\$(stats solo 'm[0][\"requests\"]')\" -ge 1 ]"
expect "list_agents shows what was observed" bash -c "agent_status solo observed | grep -q 'first byte'"
expect "solo is busy (instances: max 1) and its idle session is listed" bash -c "[ \"\$(agent_status solo status)\" = busy ] && agent_status solo idle_sessions | grep -q $S1"
expect "an agent nobody uses is ready" bash -c "[ \"\$(agent_status broke status)\" = ready ]"

say "a spawn that waits for room"
C=$(spawn_id qcoord --coordinator -p "start")
wait_for 60 "the coordinator's spawn of solo is queued" bash -c "curl -s -H \"Authorization: Bearer \$MOO_TOKEN\" http://127.0.0.1:7700/api/v1/spawn-queue | grep -q '\"agent\":\"solo\"'"
expect "list_agents counts it" bash -c "[ \"\$(agent_status solo queued)\" = 1 ]"
$O stop "$S1" >/dev/null
wait_for 60 "once the first solo stopped, the queued one started for the coordinator" bash -c "$O sessions --json | python3 -c 'import sys,json;sys.exit(0 if any(s[\"agent\"]==\"solo\" and s[\"parent\"]==\"$C\" for s in json.load(sys.stdin)) else 1)'"
wait_for 60 "and the coordinator was told" bash -c "[ \"\$(sess 's[\"last_text\"]' $C)\" = dequeued ]"
expect "the queue is empty again" bash -c "[ \"\$(curl -s -H \"Authorization: Bearer \$MOO_TOKEN\" http://127.0.0.1:7700/api/v1/spawn-queue)\" = '[]' ]"

say "credits running out"
B=$(spawn_id broke -p "go")
wait_for 90 "a credits_exhausted event is recorded" bash -c "$O providers --events --json | grep -q credits_exhausted"
expect "the operator is notified" bash -c "$O notifications --json | grep -q 'out of credits'"
expect "the model's stats remember when" bash -c "[ -n \"\$(stats broke 'm[0][\"learned\"][\"last_credits_exhausted\"] or \"\"')\" ]"
wait_for 30 "the key's allowance is held back (source: credits)" bash -c "$O quotas --json | python3 -c 'import sys,json;sys.exit(0 if any(q[\"provider\"]==\"mock\" and q[\"exhausted\"] and q[\"source\"]==\"credits\" for q in json.load(sys.stdin)) else 1)'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
