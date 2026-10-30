#!/usr/bin/env bash
# Phase 2 e2e: host and instance resource stats; soft disk limit.
source "$(dirname "$0")/lib.sh"

say "hub + worker-b"
start_hub
join_node worker-b >/dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
sed -e 's/^name: builder/name: tiny/' -e 's/^container:.*/container: { memory: 1g, cpus: 1.0, pids: 512, disk: 1m }/' "$W/worker-b/agents/builder.yaml" > "$W/worker-b/agents/tiny.yaml"
start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b

say "instance and host stats"
SID=$($O spawn builder --node worker-b -p "Write greet(name) in greet.py with pytest tests." --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
wait_for 60 "instance stats arrive (memory, processes, disk)" bash -c "$O stats --json | python3 -c 'import sys,json;s=[i for i in json.load(sys.stdin)[\"instances\"] if i[\"session\"]==\"$SID\"];sys.exit(0 if s and s[0][\"mem_bytes\"] and s[0][\"pids\"]>=2 and s[0][\"disk_bytes\"] else 1)'"
wait_for 30 "CPU% from the second sample on" bash -c "$O stats --json | python3 -c 'import sys,json;s=[i for i in json.load(sys.stdin)[\"instances\"] if i[\"session\"]==\"$SID\"];sys.exit(0 if s and s[0][\"cpu_pct\"] is not None else 1)'"
$O stats --json | python3 -c 'import sys,json;h={x["node"] for x in json.load(sys.stdin)["hosts"]};sys.exit(0 if {"hub","worker-b"}<=h else 1)' && pass "both hosts report memory/disk" || fail "both hosts report memory/disk"
$O stats | sed 's/^/  /'
wait_for 30 "per-session history builds up" bash -c "curl -s -H 'Authorization: Bearer $MOO_TOKEN' http://127.0.0.1:7700/api/v1/sessions/$SID/stats | python3 -c 'import sys,json;d=json.load(sys.stdin);sys.exit(0 if len(d[\"recent\"])>=3 and len(d[\"minutes\"])>=1 else 1)'"

say "soft disk limit (tiny: container.disk 1m)"
TID=$($O spawn tiny --node worker-b -p "Write greet(name) in greet.py with pytest tests." --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
wait_for 90 "tiny is interrupted with a disk-limit error" bash -c "$O show $TID --json | python3 -c 'import sys,json;s=json.load(sys.stdin);sys.exit(0 if \"disk limit exceeded\" in (s.get(\"error\") or \"\") else 1)'"
$O show "$TID" --json | python3 -c 'import sys,json;s=json.load(sys.stdin);print("  state:", s["state"], "| error:", s["error"])'
$O events --session "$TID" --limit 500 | contains disk.exceeded && pass "disk.exceeded event recorded" || fail "disk.exceeded event recorded"
$O stats --json | python3 -c 'import sys,json;s=[i for i in json.load(sys.stdin)["instances"] if i["session"]=="'"$TID"'"];sys.exit(0 if s and s[0]["disk_limit"]==1048576 else 1)' && pass "stats show the 1 MiB disk limit" || fail "stats show the 1 MiB disk limit"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
