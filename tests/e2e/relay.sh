#!/usr/bin/env bash
# Phase 2 e2e: via_node egress. worker-b's builder sends its LLM calls out
# through relay-2, which leaves from 127.0.0.2 (egress_bind); the mock LLM
# records the client address it sees.
source "$(dirname "$0")/lib.sh"

say "hub, worker-b, relay-2"
start_hub
$O allow add 127.0.0.1 --note "test machine" >/dev/null
join_node worker-b > /dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
cat > "$W/worker-b/routes.yaml" <<YAML
routes:
  via-relay: { type: via_node, node: relay-2 }
YAML
sed -i 's/^route:.*//' "$W/worker-b/agents/builder.yaml"
echo "route: via-relay" >> "$W/worker-b/agents/builder.yaml"
join_node relay-2 --relay > /dev/null && $O node approve relay-2 >/dev/null
cat >> "$W/relay-2/node.yaml" <<YAML
egress_bind: 127.0.0.2
relay_ports: [8099]
YAML
start_node worker-b; start_node relay-2
wait_for 20 "worker-b connected" is_connected worker-b
wait_for 20 "relay-2 connected" is_connected relay-2
expect "worker-b's builder config is valid with a via_node route" bash -c "$BIN/moocher-node --config-dir $W/worker-b check | grep -q '✓ agent builder'"

say "LLM calls leave through the relay's IP"
SID=$($O spawn builder --node worker-b -p "Write greet(name) in greet.py with pytest tests." --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
wait_for 120 "builder on worker-b finishes a turn via relay-2" bash -c "$O show $SID --json | python3 -c 'import sys,json;s=json.load(sys.stdin);sys.exit(0 if s[\"state\"]==\"idle\" and s[\"tokens\"][\"input\"]>0 else 1)'"
CLIENTS=$(curl -s http://127.0.0.1:8099/v1/_log | python3 -c 'import sys,json;print(sorted({e["client"] for e in json.load(sys.stdin) if e["path"].endswith("/chat/completions")}))')
echo "  mock saw chat requests from: $CLIENTS"
[ "$CLIENTS" = "['127.0.0.2']" ] && pass "provider saw only the relay's address" || fail "provider saw only the relay's address"
AUTH=$(curl -s http://127.0.0.1:8099/v1/_log | python3 -c 'import sys,json;print({e["auth"] for e in json.load(sys.stdin)} == {"Bearer '"$MOCK_API_KEY"'"})')
[ "$AUTH" = True ] && pass "real key injected by worker-b (relay only forwards bytes)" || fail "real key injected by worker-b"
expect "relay-2 holds no provider key file or agents" bash -c "[ ! -d $W/relay-2/agents ]"
M=$(curl -s -H "Authorization: Bearer $MOO_TOKEN" http://127.0.0.1:7700/metrics | grep 'moo_relay_bytes_total{relay="relay-2",direction="in"}' | awk '{print $2}')
[ "${M:-0}" -gt 0 ] && pass "hub counts relayed bytes ($M in)" || fail "hub counts relayed bytes"

say "relay limits and failures"
python3 -I - "$W/relay-2/node.yaml" <<'PY'
import sys
p = sys.argv[1]
t = open(p).read().replace("relay_ports: [8099]", "relay_ports: [443]")
open(p, "w").write(t)
PY
kill "$PID_relay_2"; sleep 1; start_node relay-2
wait_for 20 "relay-2 back (now only port 443)" is_connected relay-2
TURNS=$($O events --session "$SID" --limit 1000 --json | grep -c session.turn_ended || true)
$O send "$SID" "Also add a docstring." >/dev/null
wait_for 60 "session shows OpenCode retrying while the relay refuses" bash -c "$O show $SID --json | python3 -c 'import sys,json;s=json.load(sys.stdin);sys.exit(0 if \"retrying\" in (s.get(\"current_task\") or \"\") else 1)'"
$O show "$SID" --json | python3 -c 'import sys,json;print("  current task:", json.load(sys.stdin)["current_task"][:200])'
grep -q "not in this relay's relay_ports" "$W/node-worker-b.log" && pass "worker-b logs the relay's reason" || fail "worker-b logs the relay's reason"
$O show "$SID" --json | contains "relay_ports" && pass "the reason reaches the session (keyproxy error text)" || fail "the reason reaches the session"
sed -i 's/relay_ports: \[443\]/relay_ports: [8099]/' "$W/relay-2/node.yaml"
kill "$PID_relay_2"; sleep 1; start_node relay-2
wait_for 20 "relay-2 back with port 8099 allowed" is_connected relay-2
wait_for 90 "the session recovers by itself and finishes its turn" bash -c "[ \$($O events --session $SID --limit 1000 --json | grep -c session.turn_ended) -gt $TURNS ]"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
