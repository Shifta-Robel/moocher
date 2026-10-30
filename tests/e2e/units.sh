#!/usr/bin/env bash
# Provider units (quota.units): what a call costs comes from a header or the
# usage, what one unit buys is learned per model, a dashboard reading calibrates
# a unit nobody reports, and used-up pools hold back the models that draw on them.
source "$(dirname "$0")/lib.sh"

say "hub with a provider that meters in its own units"
start_hub
mock_script small <<'JSON'
[{"text": "small done", "headers": {"x-neurons": 1}}]
JSON
mock_script big <<'JSON'
[{"text": "big done", "usage": {"cost": 60}}]
JSON
mock_prices small big
python3 -I - "$W/hub/providers.yaml" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("    prices:\n", """    quota:
      units:
        neurons:
          limits: [{ max: 100, window: 1d, at: "00:00" }]
          header: x-neurons
          usage_field: cost
        credits:
          per_model: true
        pm:
          per_model: true
          limits: [{ max: 10, window: 1d, at: "00:00" }]
    prices:
""", 1)
open(p, "w").write(s)
PY
agent_yaml "$W/hub" small small
agent_yaml "$W/hub" big big
wait_for 20 "agents loaded" bash -c "$O agents | grep -q big"
unit() { $O quotas --json | python3 -c "
import sys, json
q = [x for x in json.load(sys.stdin) if x['kind'] == '$1']
print($2 if q else '')"; }
rate() { unit "$1" "next((r['units_per_mtok'] for r in q[0]['unit']['rates'] if r['model'] == '$2'), '')"; }
agent_status() { curl -s -H "Authorization: Bearer $MOO_TOKEN" http://127.0.0.1:7700/api/v1/agents/availability | python3 -c "import sys,json;a=[x for x in json.load(sys.stdin) if x['agent']=='$1'];print(a[0]['status'] if a else '')"; }
export -f unit rate agent_status

say "the cost of a call, and what a unit buys per model"
S=$(spawn_id small -p "hello")
wait_for 60 "small answered" bash -c "[ \"\$(sess 's[\"last_text\"]' $S)\" = 'small done' ]"
wait_for 20 "the header's cost is counted" bash -c "[ -n \"\$(rate unit:neurons small)\" ]"
expect "the pool says it is measured" bash -c "[ \"\$(unit unit:neurons 'q[0][\"source\"]')\" = measured ]"
B=$(spawn_id big -p "hello")
wait_for 60 "big answered" bash -c "[ \"\$(sess 's[\"last_text\"]' $B)\" = 'big done' ]"
wait_for 20 "the usage's cost is counted in the same pool" bash -c "[ -n \"\$(rate unit:neurons big)\" ]"
expect "the models cost differently" python3 -c "import sys;sys.exit(0 if float('$(rate unit:neurons big)') > 5 * float('$(rate unit:neurons small)') else 1)"
expect "moo quotas says what a unit buys" bash -c "$O quotas | grep -q 'big: .* 1 neurons buys ~'"

say "calibrating a unit nobody reports"
wait_for 20 "one pool per model" bash -c "[ -n \"\$(unit unit:credits:small 'q[0][\"kind\"]')\" ] && [ -n \"\$(unit unit:credits:big 'q[0][\"kind\"]')\" ]"
expect "nothing is known yet" bash -c "[ \"\$(unit unit:credits:small 'q[0][\"unit\"][\"used\"]')\" = 0.0 ]"
expect_fail "several pools: the CLI asks which" $O quota set mock --unit credits --used 500
expect_fail "a unit without an amount is refused" $O quota set mock --unit credits
expect_fail "an unknown unit is refused" $O quota set mock --unit nope --used 5
expect "the dashboard reading is accepted" $O quota set mock --unit credits --used 500 --model small
wait_for 20 "it is the pool's usage now" bash -c "[ \"\$(unit unit:credits:small 'q[0][\"unit\"][\"used\"]')\" = 500.0 ]"
expect "and teaches that model's rate" bash -c "[ \"\$(unit unit:credits:small 'q[0][\"unit\"][\"rates\"][0][\"source\"]')\" = calibrated ]"
expect "the other model's pool is untouched" bash -c "[ \"\$(unit unit:credits:big 'q[0][\"unit\"][\"used\"]')\" = 0.0 ]"
expect "overrides.yaml does not keep it" bash -c "! grep -qs credits $W/hub/overrides.yaml"

say "a per-model pool holds back only its model"
expect "both agents can start" bash -c "[ \"\$(agent_status big)\" = ready ] && [ \"\$(agent_status small)\" != wait_allowance ]"
expect "big's pool is used up" $O quota set mock --unit pm --used 10 --model big
wait_for 20 "the pool says so" bash -c "[ \"\$(unit unit:pm:big 'q[0][\"exhausted\"]')\" = True ]"
wait_for 20 "big waits for its allowance" bash -c "[ \"\$(agent_status big)\" = wait_allowance ]"
expect "small does not" bash -c "[ \"\$(agent_status small)\" != wait_allowance ]"
expect "a notification names the provider" bash -c "$O notifications --json | grep -q 'mock allowance used up'"
$O send "$B" "again" >/dev/null 2>&1
wait_for 60 "the running big session parks" bash -c "$O events --session $B --limit 1000 | grep -q 'quota.parked'"
$O send "$S" "again" >/dev/null 2>&1
wait_for 60 "the small session carries on" bash -c "[ \"\$(sess 's[\"turns\"]' $S)\" = 2 ]"

say "a shared pool holds back every model"
expect "the shared pool is used up" $O quota set mock --unit neurons --used 100
wait_for 20 "small waits now too" bash -c "[ \"\$(agent_status small)\" = wait_allowance ]"

stop_all
[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
