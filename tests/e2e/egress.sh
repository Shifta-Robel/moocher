#!/usr/bin/env bash
# Phase 5 e2e: `container.egress: allowlist`. The agent's shell reaches the
# network only through the node's proxy, and only for listed hosts. (With
# the `process` runtime nothing stops a direct connection; the probe uses the
# proxy explicitly, as everything in a --network=none container must.)
source "$(dirname "$0")/lib.sh"

say "hub; an agent that may reach 127.0.0.1:8099 and localhost:8099 only"
start_hub
python3 -I -m http.server 8098 --bind 127.0.0.1 --directory "$W" > "$W/other.log" 2>&1 &
PIDS+=($!)
cat > "$W/probe.sh" <<'SH'
#!/bin/sh
# Every request goes through $HTTP_PROXY, like it would in a container.
c() { curl -s -o /dev/null -w '%{http_code}' --noproxy '' -x "$HTTP_PROXY" "$@"; }
t() { curl -s -o /dev/null -w '%{http_connect}' --noproxy '' -x "$HTTP_PROXY" -p "$@"; } # the proxy's answer to CONNECT
{
  echo "env $(env | grep -c '^HTTPS\?_PROXY=http://127.0.0.1:')"
  echo "allowed $(c http://127.0.0.1:8099/v1/ip)"
  echo "tunnel $(t http://127.0.0.1:8099/v1/ip)"
  echo "unlisted $(c http://127.0.0.1:8098/)"
  echo "unlisted_tunnel $(t http://127.0.0.1:8098/)"
  echo "name_to_loopback $(c http://localhost:8099/v1/ip)"
} > probe.txt
SH
mock_script megress <<JSON
[{"tool": "bash", "args": {"command": "sh $W/probe.sh", "description": "probe egress"}}, {"text": "probed"}]
JSON
agent_yaml "$W/hub" megress megress "container: { egress: allowlist, allow_hosts: [\"127.0.0.1:8099\", \"localhost:8099\"] }"
sed -i 's/allow: \["python3 \*"/allow: ["sh *", "python3 *"/' "$W/hub/agents/megress.yaml"
wait_for 20 "agent loaded" bash -c "$O agents --json | python3 -c 'import sys,json;sys.exit(0 if any(a[\"agent\"][\"name\"]==\"megress\" for a in json.load(sys.stdin)) else 1)'"

say "a profile with an allowlist and no hosts is refused"
agent_yaml "$W/hub" mbad megress "container: { egress: allowlist }"
wait_for 20 "the bad profile is reported" bash -c "$O agents --json | grep -q 'needs container.allow_hosts' || grep -q 'needs container.allow_hosts' $W/node-hub.log"
rm "$W/hub/agents/mbad.yaml"

say "the agent's shell goes through the proxy"
SID=$(spawn_id megress -p "Probe the network.")
WS="$W/hub/data/workspaces"
wait_for 120 "probe ran" bash -c "[ -s $WS/$SID/probe.txt ] && grep -q name_to_loopback $WS/$SID/probe.txt"
sed 's/^/  /' "$WS/$SID/probe.txt"
v() { awk -v k="$1" '$1==k{print $2}' "$WS/$SID/probe.txt"; }
[ "$(v env)" = 2 ] && pass "HTTP_PROXY and HTTPS_PROXY point at the bridge" || fail "proxy env set (got $(v env))"
[ "$(v allowed)" = 200 ] && pass "a listed host:port is reachable (plain HTTP)" || fail "listed host reachable (got $(v allowed))"
[ "$(v tunnel)" = 200 ] && pass "and through CONNECT" || fail "listed host via CONNECT (got $(v tunnel))"
[ "$(v unlisted)" = 403 ] && pass "an unlisted port is refused (403)" || fail "unlisted refused (got $(v unlisted))"
[ "$(v unlisted_tunnel)" = 403 ] && pass "and its CONNECT too" || fail "unlisted CONNECT refused (got $(v unlisted_tunnel))"
[ "$(v name_to_loopback)" = 403 ] && pass "a listed name that resolves to loopback is refused" || fail "name to loopback refused (got $(v name_to_loopback))"
[ ! -s "$W/other.log" ] || ! grep -q "GET /" "$W/other.log" && pass "the unlisted server saw no request" || fail "the unlisted server saw no request"
wait_for 10 "egress.denied recorded with the reason" bash -c "$O events --session $SID --limit 1000 | grep egress.denied | grep -q 'not in this agent'"
$O events --session "$SID" --limit 1000 | grep -q "internal address" && pass "the loopback refusal says why" || fail "the loopback refusal says why"

say "the allowlist is live: remove a host and it is refused on the next connection"
sed -i 's/"127.0.0.1:8099", //' "$W/hub/agents/megress.yaml"
sleep 3
SOCK=$(ls "$(run_dir hub)"/*/proxy.sock 2>/dev/null | head -1)
python3 -I - "$SOCK" <<'PY' > "$W/live.txt"
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
s.sendall(b"GET http://127.0.0.1:8099/v1/ip HTTP/1.1\r\nHost: 127.0.0.1:8099\r\n\r\n")
print(s.recv(200).split(b"\r\n")[0].decode())
PY
grep -q " 403 " "$W/live.txt" && pass "127.0.0.1:8099 refused after the edit: $(cat "$W/live.txt")" || fail "refused after the edit (got $(cat "$W/live.txt"))"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
