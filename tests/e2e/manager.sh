#!/usr/bin/env bash
# The manager: answers from live reads, keeps nothing, only reads, and only
# ever sends the browser to pages of the dashboard.
export ENROLL_YAML="manager: { agent: mgr }"
source "$(dirname "$0")/lib.sh"

say "hub with a manager"
start_hub
mock_script mgrm <<'JSON'
{"phases": [
  {"match": "how many", "steps": [{"tool": "cluster_overview", "args": {}}, {"tool": "list_sessions", "args": {}}, {"text": "ZEBRA-ANSWER"}]},
  {"match": "take me", "steps": [{"tool": "navigate", "args": {"path": "/quotas", "label": "Quotas"}}, {"text": "There."}]},
  {"match": "offer", "steps": [{"tool": "navigate", "args": {"path": "/work", "label": "Work map", "show_only": true}}, {"text": "See the map."}]},
  {"match": "evil", "steps": [{"tool": "navigate", "args": {"path": "https://evil.example/", "label": "x"}}, {"text": "Could not."}]},
  {"match": "loop", "steps": [
    {"tool": "questions", "args": {}}, {"tool": "questions", "args": {}}, {"tool": "questions", "args": {}}, {"tool": "questions", "args": {}},
    {"tool": "questions", "args": {}}, {"tool": "questions", "args": {}}, {"tool": "questions", "args": {}}, {"tool": "questions", "args": {}},
    {"tool": "questions", "args": {}}, {"tool": "questions", "args": {}}, {"text": "never reached"}]},
  {"steps": [{"text": "plain answer"}]}
]}
JSON
mock_prices mgrm
cat > "$W/hub/agents/mgr.yaml" <<'YAML'
name: mgr
kind: generic
provider: { id: mock, model: mgrm, key: { ref: "env:MOCK_API_KEY" } }
YAML
wait_for 30 "agent loaded" bash -c "$O agents | grep -q mgr"
API=http://127.0.0.1:7700/api/v1
ask() { curl -s -H "Authorization: Bearer $MOO_TOKEN" -H 'content-type: application/json' -d "$1" "$API/manager/ask"; }
field() { python3 -c "import sys,json;print(json.load(sys.stdin)$1)"; }
log() { curl -s http://127.0.0.1:8099/v1/_log; }
export -f ask field log
export API

say "it answers from what it reads now"
expect "it is ready" bash -c "curl -s -H \"Authorization: Bearer \$MOO_TOKEN\" $API/manager | grep -q '\"ready\":true'"
R=$(ask '{"question": "how many sessions? QUOKKA-99"}')
expect "the answer comes back" bash -c "echo '$R' | field '[\"answer\"]' | grep -q ZEBRA-ANSWER"
expect "after reading the cluster" bash -c "echo '$R' | field '[\"tools\"]' | grep -q list_sessions"
expect "the call is priced" python3 -c "import json;assert json.loads('''$R''')['usage']['cost_usd']>0"
expect "the CLI asks too" bash -c "$O ask 'plain please' | grep -q 'plain answer'"
expect "it took no instance" bash -c "[ \"\$($O sessions --json | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')\" = 0 ]"

say "it keeps nothing"
ask '{"question": "fresh question", "history": [{"role":"user","text":"earlier"},{"role":"assistant","text":"earlier answer"}]}' >/dev/null
expect "a question carries only the text it was sent" bash -c "log | python3 -c \"import sys,json;m=json.load(sys.stdin)[-1]['messages'];print([x['role'] for x in m])\" | grep -qF \"['system', 'user', 'assistant', 'user']\""
expect "tool results from earlier questions are gone" bash -c "! log | python3 -c \"import sys,json;print(json.dumps(json.load(sys.stdin)[-1]['messages']))\" | grep -q 'sessions_by_state'"
sleep 1
expect "the question is nowhere on the hub" bash -c "! grep -rl QUOKKA-99 $W/hub --include=* 2>/dev/null | grep -q ."
expect "only that it was asked is audited" bash -c "$O audit --limit 50 2>/dev/null | grep -q manager.ask || curl -s -H \"Authorization: Bearer \$MOO_TOKEN\" $API/audit | grep -q manager.ask"

say "it goes only to dashboard pages"
R=$(ask '{"question": "take me to the allowances"}')
expect "it navigates" bash -c "[ \"\$(echo '$R' | field '[\"navigate\"]')\" = /quotas ]"
R=$(ask '{"question": "offer the map"}')
expect "show_only offers a link without moving" bash -c "[ \"\$(echo '$R' | field '[\"navigate\"]')\" = None ] && echo '$R' | grep -q '/work'"
R=$(ask '{"question": "evil page"}')
expect "another site is refused" bash -c "[ \"\$(echo '$R' | field '[\"navigate\"]')\" = None ] && ! echo '$R' | grep -q evil.example"

say "it is bounded"
R=$(ask '{"question": "loop forever"}')
expect "tool rounds stop by themselves" bash -c "[ \"\$(echo '$R' | field '[\"tools\"].__len__()')\" = 8 ] && echo '$R' | grep -q 'could not find'"
BIG=$(python3 -c "print('x'*5000)")
expect "a huge question is refused" bash -c "ask '{\"question\": \"$BIG\"}' | grep -q 'too long'"
TOK=$(curl -s -H "Authorization: Bearer $MOO_TOKEN" -H 'content-type: application/json' -d '{"name":"reader","scope":"read"}' "$API/tokens" | field '["token"]')
expect "a read-only token may not spend money on it" bash -c "curl -s -H 'Authorization: Bearer $TOK' -H 'content-type: application/json' -d '{\"question\":\"hi\"}' $API/manager/ask | grep -q insufficient"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
