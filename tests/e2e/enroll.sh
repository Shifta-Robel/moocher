#!/usr/bin/env bash
# Phase 2 e2e: remote enrollment over mTLS, approval, sessions on a remote
# node, relay allowlist, revocation, token reuse and CA pinning.
source "$(dirname "$0")/lib.sh"

say "hub up"
start_hub
wait_for 10 "hub node connected over unix socket" is_connected hub

say "worker-b: invite + join"
join_node worker-b > "$W/join-b.txt"
cat "$W/join-b.txt"
expect "node.yaml written by join" test -f "$W/worker-b/node.yaml"
expect "node key is 0600" bash -c "[ \$(stat -c %a $W/worker-b/pki/node.key) = 600 ]"
[ "$(node_field worker-b enrollment)" = pending ] && pass "worker-b is pending after enroll" || fail "worker-b is pending after enroll"

say "join token is single use"
TOK=$(grep -o 'moojoin\.[^ ]*' "$W/join-b.txt" || true)
CMD=$($O node invite worker-c --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
expect "worker-c enrolls with its token" "$BIN/moocher-node" --config-dir "$W/worker-c" join https://127.0.0.1:7443 --token "$CMD"
expect_fail "re-using worker-c's token is refused" "$BIN/moocher-node" --config-dir "$W/worker-c2" join https://127.0.0.1:7443 --token "$CMD"

say "CA pinning: a token with the wrong CA fingerprint is refused before anything is sent"
BAD=$($O node invite worker-d --json | python3 -c 'import sys,json;t=json.load(sys.stdin)["token"];a,s,f=t.split(".");print(a+"."+s+"."+"0"*64)')
expect_fail "wrong CA fingerprint refused" "$BIN/moocher-node" --config-dir "$W/worker-d" join https://127.0.0.1:7443 --token "$BAD"

say "pending node cannot attach until approved"
make_worker worker-b
start_node worker-b
sleep 3
expect "pending worker-b is not connected" is_disconnected worker-b
grep -q "awaiting operator approval" "$W/node-worker-b.log" && pass "node logs the approval hint" || fail "node logs the approval hint"
$O node approve worker-b
wait_for 20 "worker-b connects after approval" is_connected worker-b
[ "$(node_field worker-b observed_ip)" = 127.0.0.1 ] && pass "hub records worker-b's source IP" || fail "hub records worker-b's source IP"

say "a session runs on the remote node"
SID=$($O spawn builder --node worker-b -p "Write greet(name) in greet.py with pytest tests." --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
wait_for 120 "remote builder finishes a turn" bash -c "$O show $SID --json | python3 -c 'import sys,json;s=json.load(sys.stdin);sys.exit(0 if s[\"state\"]==\"idle\" and s[\"tokens\"][\"input\"]>0 else 1)'"
expect "greet.py written in worker-b's workspace" test -f "$W/worker-b/data/workspaces/$SID/greet.py"

say "relay nodes need an allowed source address"
expect_fail "relay enroll from a non-allowed IP is refused" join_node relay-1 --relay
$O allow add 127.0.0.1 --note "test machine"
expect "relay enrolls once 127.0.0.1 is allowed" join_node relay-2 --relay
grep -q "role: relay" "$W/relay-2/node.yaml" && pass "relay node.yaml has role: relay" || fail "relay node.yaml has role: relay"
$O node approve relay-2
start_node relay-2
wait_for 20 "relay-2 connects" is_connected relay-2
[ "$(node_field relay-2 role)" = relay ] && pass "relay-2 shows role relay" || fail "relay-2 shows role relay"
expect_fail "spawning on a relay node is refused" $O spawn builder --node relay-2
$O allow rm 127.0.0.1
wait_for 10 "removing the allowlist entry disconnects relay-2" is_disconnected relay-2
sleep 3
expect "relay-2 stays out while not allowed" is_disconnected relay-2
expect "worker nodes are not affected by the relay allowlist" is_connected worker-b

say "certificate renewal (backdate the cert metadata so renewal is due)"
FP1=$(sha256sum "$W/worker-b/pki/node.crt" | cut -c1-16)
python3 -I -c "import json,sys;p=sys.argv[1];m=json.load(open(p));m['issued_at']=0;json.dump(m,open(p,'w'))" "$W/worker-b/pki/node.json"
kill "$PID_worker_b"; sleep 1
start_node worker-b
wait_for 20 "worker-b reconnects" is_connected worker-b
wait_for 20 "node renewed its certificate" bash -c "[ \$(sha256sum $W/worker-b/pki/node.crt | cut -c1-16) != $FP1 ]"
kill "$PID_worker_b"; sleep 1
start_node worker-b
wait_for 20 "worker-b connects with the renewed certificate" is_connected worker-b
grep -q "link certificate renewed" "$W/node-worker-b.log" || true

say "revocation"
$O node revoke worker-b
wait_for 10 "revoked worker-b is disconnected" is_disconnected worker-b
sleep 4
expect "revoked worker-b cannot reconnect" is_disconnected worker-b
grep -q "revoked" "$W/node-worker-b.log" && pass "node logs the revocation" || fail "node logs the revocation"

say "audit"
$O audit --limit 40 | grep -E 'node\.|allowlist' | sed 's/^/  /'
expect "audit chain verifies" bash -c "$O audit --verify | grep -q '\"ok\": true'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
