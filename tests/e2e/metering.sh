#!/usr/bin/env bash
# Phase 5 e2e: keyproxy's own usage meter. Budgets count what the provider
# reports in its responses, not only what OpenCode says it used, so calls made
# around OpenCode (the agent has a shell and the capability token) are billed.
source "$(dirname "$0")/lib.sh"

say "hub with a metered agent (budget \$0.05)"
start_hub
mock_script mmeter <<'JSON'
[{"text": "hello from the metered agent"}]
JSON
mock_prices mmeter
agent_yaml "$W/hub" mmeter mmeter
sed -i 's/^limits: { max_sessions: 4 }$/limits: { max_sessions: 4, budget_usd: 0.05 }/' "$W/hub/agents/mmeter.yaml"
wait_for 20 "agent loaded" bash -c "$O agents --json | python3 -c 'import sys,json;sys.exit(0 if any(a[\"agent\"][\"name\"]==\"mmeter\" for a in json.load(sys.stdin)) else 1)'"

say "OpenCode's count and the meter agree on an ordinary turn"
SID=$(spawn_id mmeter -p "Say hello.")
wait_for 90 "turn ended" bash -c "[ \"\$(sess_state $SID)\" = idle ]"
wait_for 10 "metered tokens equal OpenCode's" bash -c "[ \"\$(sess 's[\"metered\"][\"input\"]+s[\"metered\"][\"output\"]' $SID)\" = \"\$(sess 's[\"tokens\"][\"input\"]+s[\"tokens\"][\"output\"]' $SID)\" ] && [ \"\$(sess 's[\"metered\"][\"input\"]>0' $SID)\" = True ]"
sess '"  OpenCode: %d tok $%.5f  metered: %d tok $%.5f" % (s["tokens"]["input"]+s["tokens"]["output"], s["tokens"]["cost_usd"], s["metered"]["input"]+s["metered"]["output"], s["metered"]["cost_usd"])' "$SID"
curl -s http://127.0.0.1:8099/v1/_log | python3 -c 'import sys,json;l=[e for e in json.load(sys.stdin) if e["path"].endswith("/chat/completions") and e["stream"]];sys.exit(0 if l and all(e["include_usage"] for e in l) else 1)' \
  && pass "every streamed request reached the provider asking for usage" || fail "every streamed request reached the provider asking for usage"

say "calls made around OpenCode, with the capability token from inside the instance"
CFG="$W/hub/data/instances/$SID/etc/opencode.json"
BASE=$(python3 -c 'import sys,json;c=json.load(open(sys.argv[1]));print(c["provider"]["ocl-mock"]["options"]["baseURL"])' "$CFG" 2>/dev/null || python3 -c 'import sys,json;c=json.load(open(sys.argv[1]));print(next(iter(c["provider"].values()))["options"]["baseURL"])' "$CFG")
CAP=$(python3 -c 'import sys,json;c=json.load(open(sys.argv[1]));print(next(iter(c["provider"].values()))["options"]["apiKey"])' "$CFG")
# ~50k tokens per call by the mock's count ($0.025 at $0.50/Mtok).
python3 -c 'import json;print(json.dumps({"model":"mmeter","stream":True,"messages":[{"role":"user","content":"x"*200000}]}))' > "$W/big.json"
around() { curl -s -o "$W/around.out" -w '%{http_code}' -H "authorization: Bearer $CAP" -H 'content-type: application/json' --data-binary @"$W/big.json" "$BASE/chat/completions"; }
C1=$(around); C2=$(around)
[ "$C1$C2" = 200200 ] && pass "two direct calls served (streamed)" || fail "two direct calls served (got $C1 $C2)"
wait_for 10 "the meter counts them; OpenCode's count does not move" bash -c "[ \"\$(sess 's[\"metered\"][\"input\"] > s[\"tokens\"][\"input\"] + 90000' $SID)\" = True ]"
sess '"  OpenCode: %d tok $%.5f  metered: %d tok $%.5f" % (s["tokens"]["input"]+s["tokens"]["output"], s["tokens"]["cost_usd"], s["metered"]["input"]+s["metered"]["output"], s["metered"]["cost_usd"])' "$SID"
wait_for 10 "usage.mismatch recorded" bash -c "$O events --session $SID --limit 1000 | grep -q usage.mismatch"
wait_for 10 "and raised as a warning notification" bash -c "$O notifications | grep -q 'more tokens than OpenCode reports'"
$O show "$SID" | contains "metered by keyproxy" && pass "ocl show prints the metered count" || fail "ocl show prints the metered count"
$O sessions --json | python3 -c "import sys,json;s=[x for x in json.load(sys.stdin) if x['id']=='$SID'][0];sys.exit(0 if s['metered']['cost_usd']>=0.05 else 1)" \
  && pass "metered spend is past the \$0.05 budget" || fail "metered spend is past the budget"

say "the budget is enforced on the metered count"
C3=$(around)
[ "$C3" = 429 ] && grep -q "budget" "$W/around.out" && pass "the next call is refused: $(head -c 120 "$W/around.out")" || fail "the next call is refused (got $C3)"
wait_for 20 "the budget question went to the operator" bash -c "$O questions --json | python3 -c 'import sys,json;sys.exit(0 if any(q[\"asker\"]==\"$SID\" and q[\"kind\"]==\"budget\" for q in json.load(sys.stdin)) else 1)'"

say "the meter survives a node restart"
BEFORE=$(sess 's["metered"]["input"]' "$SID")
kill "${PIDS[2]}"
wait_for 20 "hub node gone" is_disconnected hub
"$BIN/opencode-node" --config-dir "$W/hub" run >> "$W/node-hub.log" 2>&1 &
PIDS+=($!)
wait_for 30 "hub node back" is_connected hub
wait_for 30 "metered count kept" bash -c "[ \"\$(sess 's[\"metered\"][\"input\"]' $SID)\" = $BEFORE ]"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
