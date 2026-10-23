#!/usr/bin/env bash
# Phase 2 e2e: the supervisor restarts while a remote node (mTLS) is working.
# The node buffers telemetry, reconnects with backoff, replays the outbox; the
# session finishes with correct tokens and no orphan.
source "$(dirname "$0")/lib.sh"

say "hub + worker-b over mTLS"
start_hub
join_node worker-b >/dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b
SUP_PID=${PIDS[1]}

say "kill the supervisor mid-turn for 15 s"
SID=$($O spawn builder --node worker-b -p "Write greet(name) in greet.py with pytest tests." --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
wait_for 60 "builder is running" bash -c "$O show $SID --json | grep -q '\"state\": \"running\"'"
kill "$SUP_PID"; sleep 15
expect_fail "supervisor is down" $O nodes
"$BIN/ocl-supervisor" --config-dir "$W/hub" run >> "$W/supervisor.log" 2>&1 &
PIDS+=($!)
wait_for 70 "worker-b reconnects over mTLS after the restart" is_connected worker-b
grep -c '"outbox":[1-9]' "$W/node-worker-b.log" >/dev/null && pass "the node buffered events while the hub was down" || fail "the node buffered events while the hub was down"
wait_for 120 "the session's turn end arrives (replayed or live)" bash -c "[ \$($O events --session $SID --limit 1000 --json | grep -c session.turn_ended) -ge 1 ]"
M=$(curl -s http://127.0.0.1:8099/v1/_log | python3 -c 'import sys,json;print(sum(1 for e in json.load(sys.stdin) if e["path"].endswith("/chat/completions")))')
$O show "$SID" --json | python3 -c 'import sys,json;s=json.load(sys.stdin);print("  state:", s["state"], "tokens:", s["tokens"]["input"]+s["tokens"]["output"], "todos:", sum(t["status"]=="completed" for t in s["todos"]), "/", len(s["todos"]))'
$O show "$SID" --json | python3 -c 'import sys,json;s=json.load(sys.stdin);sys.exit(0 if s["state"]=="idle" and s["tokens"]["input"]>0 and all(t["status"]=="completed" for t in s["todos"]) else 1)' && pass "session state, tokens and todos are complete after the outage" || fail "session complete after the outage"
wait_for 10 "quota counts survived the restart ($M provider calls)" bash -c "$O quotas --json | python3 -c 'import sys,json;sys.exit(0 if sum(q[\"used\"] for q in json.load(sys.stdin) if q[\"kind\"]==\"requests\")==$M else 1)'"
expect "audit chain verifies across the restart" bash -c "$O audit --verify | grep -q '\"ok\": true'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
