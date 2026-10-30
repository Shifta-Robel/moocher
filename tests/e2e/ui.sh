#!/usr/bin/env bash
# Phase 4 e2e: the web UI in a real browser (headless Chromium via Playwright):
# login, overview, starting an instance, live updates, Open in OpenCode,
# answering a question, the task board, adding a machine, dark mode and the
# phone layout. Screenshots go to $MOO_E2E_DIR/shots.
# Needs PW_DIR (npm install playwright; npx playwright install chromium-headless-shell).
source "$(dirname "$0")/lib.sh"
: "${PW_DIR:?set PW_DIR to a directory with node_modules/playwright}"

ENROLL_YAML="attach_listen: 127.0.0.1:7444" start_hub
mock_script qtop <<'JSON'
{"phases": [
  {"match": "Answer to your question", "steps": [{"tool": "write", "args": {"filePath": "answer.txt", "content": "{{last_user}}"}}, {"text": "got it"}]},
  {"steps": [{"tool": "cluster_ask", "args": {"question": "Which port should the service listen on?", "options": ["8080", "9090"], "importance": "normal"}}, {"text": "asked"}]}
]}
JSON
mock_prices qtop
agent_yaml "$W/hub" qtop qtop
echo "browser-test-password" | "$BIN/moocher-hub" --config-dir "$W/hub" user set admin >/dev/null
wait_for 20 "agents loaded" bash -c "$O agents | grep -q qtop"
Q=$(spawn_id qtop -p "start")
mkdir -p "$S/shots"
node "$REPO/tests/e2e/ui-check.mjs" http://127.0.0.1:7700 admin browser-test-password "$Q" "$S/shots" > "$S/ui-check.out" 2>&1
sed 's/^/  /' "$S/ui-check.out"
expect "every browser check passed" bash -c "! grep -q '^FAIL' $S/ui-check.out && grep -q 'logout returns' $S/ui-check.out"
expect "the answer reached the agent" bash -c "grep -q 9090 $W/hub/data/workspaces/$Q/answer.txt"
[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
