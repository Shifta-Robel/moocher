#!/usr/bin/env bash
# Provider request rates and per-model limits: a call above the rate waits and
# is not refused, a model's limits and 429s stop that model only, and its tokens
# count against its own allowance.
source "$(dirname "$0")/lib.sh"

say "a key shared by several models with their own settings"
start_hub
cat >> "$W/hub/providers.yaml" <<YAML
  mockr:
    base_url: http://127.0.0.1:8099/v1
    api: openai
    quota:
      scope: key
      rate: { rpm: 600 }
      models:
        ratem: { rate: { rps: 1 } }
        capm: { limits: [{ kind: requests, max: 3, window: 1h, rolling: true }] }
        tokm: { limits: [{ kind: tokens, max: 1, window: 1h, rolling: true }] }
        limitedm: {}
YAML
call='{"tool": "cluster_read_inbox", "args": {}}'
agent() { # agent NAME MODEL
  cat > "$W/hub/agents/$1.yaml" <<YAML
name: $1
description: e2e agent $1
provider: { id: mockr, model: $2, key: { ref: "env:MOCK_API_KEY" } }
tools: { edit: allow, bash: { default: ask } }
limits: { max_sessions: 4 }
YAML
}
mock_script ratem <<JSON
[$call, $call, $call, $call, {"text": "rate done"}]
JSON
mock_script capm <<JSON
[$call, $call, $call, $call, {"text": "cap done"}]
JSON
mock_script tokm <<JSON
[$call, {"text": "token done"}]
JSON
mock_script limitedm <<'JSON'
[{"error": {"status": 429, "body": {"error": {"message": "mock: slow down"}}}}, {"text": "limited done"}]
JSON
mock_script otherm <<'JSON'
[{"text": "other here"}]
JSON
mock_prices ratem capm tokm limitedm otherm
for m in rate cap tok limited other; do agent "$m" "${m}m"; done
wait_for 30 "agents loaded" bash -c "for a in rate cap tok limited other; do $O agents | grep -qw \$a || exit 1; done"
calls() { curl -s http://127.0.0.1:8099/v1/_log | python3 -c "import sys,json;print(sum(e['model']=='$1' for e in json.load(sys.stdin)))"; }
gaps() { curl -s http://127.0.0.1:8099/v1/_log | python3 -c "
import sys,json
t=[e['ts'] for e in json.load(sys.stdin) if e['model']=='$1']
print(min(b-a for a,b in zip(t,t[1:])))"; }
export -f calls

say "above the request rate a call waits, and nothing is refused"
SR=$(spawn_id rate -p "go")
wait_for 150 "it finishes" bash -c "[ \"\$(sess 's[\"last_text\"]' $SR)\" = 'rate done' ]"
G=$(gaps ratem); echo "  shortest gap between calls: ${G}s"
expect "its calls went out at most one a second" python3 -c "assert $G >= 0.9"
expect "the wait is on record" bash -c "$O events --session $SR --limit 100 | grep -q quota.paced"
expect_fail "and it never counted as used up" bash -c "$O events --session $SR --limit 100 | grep -q quota.parked"

say "one model's limit stops that model, not the others on the key"
SC=$(spawn_id cap -p "go")
wait_for 150 "it stops at its limit" bash -c "[ \"\$(sess_state $SC)\" = parked ]"
expect "after 3 calls" bash -c "[ \"\$(calls capm)\" = 3 ]"
wait_for 15 "the limit shows on its own line" bash -c "$O quotas --json | grep -q '|capm'"
SO=$(spawn_id other -p "go")
wait_for 120 "another model on the key still answers" bash -c "[ \"\$(sess 's[\"last_text\"]' $SO)\" = 'other here' ]"
expect "and the model at its limit stays stopped" bash -c "[ \"\$(sess_state $SC)\" = parked ]"

say "a model's tokens count against its own allowance"
ST=$(spawn_id tok -p "go")
wait_for 150 "it stops once they are spent" bash -c "[ \"\$(sess_state $ST)\" = parked ]"
expect "after one call" bash -c "[ \"\$(calls tokm)\" = 1 ]"

say "a 429 for a listed model stays with that model"
SL=$(spawn_id limited -p "go")
wait_for 150 "the provider's 429 parks it" bash -c "[ \"\$(sess_state $SL)\" = parked ]"
SO2=$(spawn_id other -p "go")
wait_for 120 "another model still goes out on the same key" bash -c "[ \"\$(sess 's[\"last_text\"]' $SO2)\" = 'other here' ]"
stop_all
[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
