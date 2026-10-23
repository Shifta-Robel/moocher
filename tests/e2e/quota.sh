#!/usr/bin/env bash
# Phase 2 e2e: per-IP allowances, failover to another IP, parking with a
# countdown, operator overrides. The mock allows 4 chat requests per client IP
# per 45 s and answers with x-ratelimit-* headers / 429 like a keyless gateway.
# worker-b goes out directly (127.0.0.1) or via relay-2 (127.0.0.2).
MOCK_ARGS="--ip-quota 4 --ip-window 45"
# The hub's own node reaches the mock from 127.0.0.1 too: it shares worker-b's direct IP.
HUB_NODE_YAML='ip_echo: ["http://127.0.0.1:8099/v1/ip"]'

source "$(dirname "$0")/lib.sh"
SESS_FIELD() { $O show "$1" --json | python3 -c "import sys,json;print(json.load(sys.stdin).get('$2') or '')"; }

say "cluster: hub, worker-b (direct + failover via relay-2), relay-2 on 127.0.0.2"
start_hub
$O allow add 127.0.0.1 >/dev/null
# The mock counts per IP, like a keyless gateway.
cat >> "$W/hub/providers.yaml" <<YAML
    quota: { scope: ip }
YAML
join_node worker-b > /dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
cat > "$W/worker-b/routes.yaml" <<YAML
routes:
  via-relay: { type: via_node, node: relay-2 }
YAML
echo 'ip_echo: ["http://127.0.0.1:8099/v1/ip"]' >> "$W/worker-b/node.yaml"
echo 'route: { prefer: direct, failover: [via-relay] }' >> "$W/worker-b/agents/builder.yaml"
join_node relay-2 --relay > /dev/null && $O node approve relay-2 >/dev/null
printf 'egress_bind: 127.0.0.2\nrelay_ports: [8099]\n' >> "$W/relay-2/node.yaml"
start_node relay-2; start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b
wait_for 20 "relay-2 connected" is_connected relay-2

say "egress IPs per route"
$O node probe worker-b >/dev/null
wait_for 20 "worker-b knows both routes' IPs" bash -c "$O ips --json | python3 -c 'import sys,json;e=json.load(sys.stdin);m={(i[\"node\"],i[\"route\"]):i[\"ip\"] for i in e[\"ips\"]};sys.exit(0 if m.get((\"worker-b\",\"direct\"))==\"127.0.0.1\" and m.get((\"worker-b\",\"via-relay\"))==\"127.0.0.2\" else 1)'"
$O ips | sed 's/^/  /'
$O ips --json | python3 -c 'import sys,json;e=json.load(sys.stdin);s=[x for x in e["shared"] if x["ip"]=="127.0.0.1"];sys.exit(0 if s and sorted(s[0]["nodes"])==["hub","worker-b"] else 1)' && pass "127.0.0.1 is shown as shared by hub and worker-b" || fail "127.0.0.1 is shown as shared by hub and worker-b"

say "the direct allowance runs out mid-turn; keyproxy fails over to the relay's IP"
SID=$($O spawn builder --node worker-b -p "Write greet(name) in greet.py with pytest tests." --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
wait_for 150 "builder finishes its turn" bash -c "[ \$($O events --session $SID --limit 1000 --json | grep -c session.turn_ended) -ge 1 ]"
CL=$(curl -s http://127.0.0.1:8099/v1/_log | python3 -c 'import sys,json;from collections import Counter;print(dict(Counter(e["client"] for e in json.load(sys.stdin) if e["path"].endswith("/chat/completions"))))')
echo "  provider saw: $CL"
python3 -I -c "import ast,sys;d=ast.literal_eval(sys.argv[1]);sys.exit(0 if d.get('127.0.0.1',0)<=5 and d.get('127.0.0.2',0)>=1 else 1)" "$CL" && pass "requests moved to 127.0.0.2 after 127.0.0.1's allowance" || fail "requests moved to 127.0.0.2"
$O events --session "$SID" --limit 1000 | contains route.failover && pass "route.failover event recorded" || fail "route.failover event recorded"
wait_for 10 "hub shows 127.0.0.1's mock allowance exhausted with a reset time" bash -c "$O quotas --json | python3 -c 'import sys,json;q=[x for x in json.load(sys.stdin) if x[\"provider\"]==\"mock\" and x[\"scope_id\"]==\"127.0.0.1\" and x[\"kind\"]==\"requests\"];sys.exit(0 if q and q[0][\"exhausted\"] and q[0][\"resets_at\"] else 1)'"
$O quotas mock | sed 's/^/  /'
wait_for 10 "hub's per-IP request counts match what the provider saw" bash -c "$O quotas --json | python3 -c 'import sys,json,ast;q={x[\"scope_id\"]:x[\"used\"] for x in json.load(sys.stdin) if x[\"provider\"]==\"mock\" and x[\"kind\"]==\"requests\"};d=ast.literal_eval(sys.argv[1]);sys.exit(0 if all(q.get(k)==v for k,v in d.items()) else 1)' \"$CL\""
wait_for 10 "tokens are attributed to the relay IP too" bash -c "$O quotas --json | python3 -c 'import sys,json;sys.exit(0 if any(x[\"scope_id\"]==\"127.0.0.2\" and x[\"kind\"]==\"tokens\" and x[\"used\"]>0 for x in json.load(sys.stdin)) else 1)'"

say "placement avoids nodes whose every route is used up (hub shares the exhausted 127.0.0.1)"
P=$($O spawn builder -p "Say hello." --json)
PN=$(echo "$P" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])')
echo "  placed on $PN: $(echo "$P" | python3 -c 'import sys,json;print(json.load(sys.stdin)["placement"][:260])')"
[ "$PN" = worker-b ] && pass "new builder placed on worker-b (it still has the relay route)" || fail "new builder placed on worker-b"
echo "$P" | contains "skipped hub" && pass "placement explains why the hub was skipped" || fail "placement explains why the hub was skipped"
$O stop "$(echo "$P" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')" >/dev/null
$O nodes | sed 's/^/  /'
$O nodes --json | python3 -c 'import sys,json;sys.exit(0 if all(n["link_rtt_ms"] is not None for n in json.load(sys.stdin) if n["connected"]) else 1)' && pass "every connected node has a link RTT" || fail "every connected node has a link RTT"

say "operator override: the relay IP is also used up for 20 s -> the session parks, then resumes"
$O quota set mock --scope-id 127.0.0.2 --remaining 0 --resets-at +20s --note "test: relay allowance gone" >/dev/null
grep -q "127.0.0.2" "$W/worker-b/overrides.yaml" && pass "override written to worker-b's overrides.yaml" || fail "override written to overrides.yaml"
TURNS=$($O events --session "$SID" --limit 1000 --json | grep -c session.turn_ended)
$O send "$SID" "Please also add a docstring to greet()." >/dev/null
wait_for 30 "session is parked while both IPs are used up" bash -c "[ \"\$(SESS_FIELD $SID state)\" = parked ] 2>/dev/null || $O show $SID --json | grep -q '\"state\": \"parked\"'"
sleep 4
echo "  current task: $($O show "$SID" --json | python3 -c 'import sys,json;print((json.load(sys.stdin).get("current_task") or "")[:220])')"
$O show "$SID" --json | python3 -c 'import sys,json;t=json.load(sys.stdin).get("current_task") or "";sys.exit(0 if t.startswith("resets in") else 1)' && pass "parked session shows the reset countdown" || fail "parked session shows the reset countdown"
wait_for 120 "session resumes by itself after the reset and finishes the turn" bash -c "[ \$($O events --session $SID --limit 1000 --json | grep -c session.turn_ended) -gt $TURNS ]"
grep -q '"""' "$W/worker-b/data/workspaces/$SID/greet.py" && pass "docstring added after resuming" || fail "docstring added after resuming"
$O events --session "$SID" --limit 1000 | contains quota.parked && pass "quota.parked event recorded" || fail "quota.parked event recorded"

say "clearing the override; live route change"
$O quota clear mock --scope-id 127.0.0.2 >/dev/null
grep -q "127.0.0.2" "$W/worker-b/overrides.yaml" && fail "override removed from overrides.yaml" || pass "override removed from overrides.yaml"
$O route set worker-b builder --prefer via-relay --failover direct >/dev/null
grep -q "prefer: via-relay" "$W/worker-b/overrides.yaml" && pass "route override written" || fail "route override written"
expect_fail "unknown route names are refused" $O route set worker-b builder --prefer nope
$O route clear worker-b builder >/dev/null
grep -q "builder" "$W/worker-b/overrides.yaml" && fail "route override cleared" || pass "route override cleared"
expect "audit chain verifies" bash -c "$O audit --verify | grep -q '\"ok\": true'"
$O audit --limit 20 | grep -E 'quota|route' | sed 's/^/  /'

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
