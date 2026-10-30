#!/usr/bin/env bash
# The work map and the manager panel in a real browser. Screenshots go to
# $MOO_E2E_DIR/shots. Needs PW_DIR (see ui.sh).
export ENROLL_YAML="manager: { agent: mgr }"
source "$(dirname "$0")/lib.sh"
: "${PW_DIR:?set PW_DIR to a directory with node_modules/playwright}"

start_hub
mock_script mapc <<'JSON'
{"phases": [
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [
    {"tool": "cluster_spawn_agent", "args": {"agent": "mapw", "prompt": "one", "title": "first worker"}},
    {"tool": "cluster_spawn_agent", "args": {"agent": "mapw", "prompt": "two", "title": "second worker"}},
    {"text": "asked"}]}
]}
JSON
mock_script mapw <<'JSON'
[{"text": "working", "delay": 200}]
JSON
mock_script mgrm <<'JSON'
{"phases": [
  {"match": "going on", "steps": [{"tool": "list_sessions", "args": {}}, {"text": "Three agents are working."}]},
  {"match": "take me", "steps": [{"tool": "navigate", "args": {"path": "/quotas", "label": "Quotas"}}, {"text": "There."}]},
  {"match": "offer", "steps": [{"tool": "navigate", "args": {"path": "/work", "label": "Work map", "show_only": true}}, {"text": "See the map."}]},
  {"match": "markup", "steps": [{"text": "<img src=x onerror=alert(1)>"}]},
  {"steps": [{"text": "plain"}]}
]}
JSON
mock_prices mapc mapw mgrm
agent_yaml "$W/hub" coordinator mapc
agent_yaml "$W/hub" mapw mapw
cat > "$W/hub/agents/mgr.yaml" <<'YAML'
name: mgr
kind: generic
provider: { id: mock, model: mgrm, key: { ref: "env:MOCK_API_KEY" } }
YAML
echo "browser-test-password" | "$BIN/moocher-hub" --config-dir "$W/hub" user set admin >/dev/null
wait_for 30 "agents loaded" bash -c "$O agents | grep -q mapw"
$O task add "Ship the release notes" --label urgent >/dev/null
mkdir -p "$S/shots"
node "$REPO/tests/e2e/map-check.mjs" http://127.0.0.1:7700 admin browser-test-password "$S/shots" > "$S/map-check.out" 2>&1 || true
sed 's/^/  /' "$S/map-check.out"
expect "every browser check passed" bash -c "! grep -q '^FAIL' $S/map-check.out && grep -q 'no page errors' $S/map-check.out"
stop_all

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
