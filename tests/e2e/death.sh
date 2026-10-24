#!/usr/bin/env bash
# Phase 3 e2e: dead agents. An instance killed mid-step is detected
# (exit:137), restarted in place with its history when that is cheap, or its
# tree is halted for the operator; recovery by restart and by resuming on
# another machine; a lost node falls back to the latest checkpoint; the
# wall-clock limit ends an instance as dead.
source "$(dirname "$0")/lib.sh"

say "hub + worker-b (checkpoints every turn, short node-lost grace)"
ENROLL_YAML="disconnect_grace_secs: 8" start_hub
mock_script dworker <<'JSON'
{"phases": [
  {"match": "stopped unexpectedly", "steps": [
    {"tool": "bash", "args": {"command": "sleep 12", "description": "the long step, again"}},
    {"tool": "write", "args": {"filePath": "b.txt", "content": "after restart"}},
    {"tool": "todowrite", "args": {"todos": [{"content": "write a", "status": "completed", "priority": "high"}, {"content": "write b", "status": "completed", "priority": "high"}]}},
    {"text": "recovered and finished"}]},
  {"match": "taking over from session", "steps": [
    {"tool": "write", "args": {"filePath": "c.txt", "content": "replacement"}},
    {"text": "replacement finished"}]},
  {"match": "was paused", "steps": [{"text": "resumed after the halt"}]},
  {"match": "continue", "steps": [{"tool": "bash", "args": {"command": "sleep 40", "description": "a long step"}}, {"text": "done"}]},
  {"match": "one turn", "steps": [
    {"tool": "write", "args": {"filePath": "a.txt", "content": "before the crash"}},
    {"text": "turn 1 done"}]},
  {"steps": [
    {"tool": "todowrite", "args": {"todos": [{"content": "write a", "status": "in_progress", "priority": "high"}, {"content": "write b", "status": "pending", "priority": "high"}]}},
    {"tool": "write", "args": {"filePath": "a.txt", "content": "before the crash"}},
    {"tool": "bash", "args": {"command": "sleep 12", "description": "a long step"}},
    {"tool": "write", "args": {"filePath": "b.txt", "content": "no crash"}},
    {"text": "finished"}]}
]}
JSON
mock_script dparent <<'JSON'
{"phases": [
  {"match": "was paused", "steps": [{"text": "parent resumed"}]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "dhalt", "prompt": "work", "title": "worker", "node": "hub"}}, {"text": "spawned"}]}
]}
JSON
mock_prices dworker dparent
# dauto: recovers by itself; dhalt: never automatically (any cost is too much).
agent_yaml "$W/hub" dauto dworker "on_death: { max_auto_fraction: 1.0 }"
agent_yaml "$W/hub" dhalt dworker "on_death: { max_auto_cost_usd: 0.0 }"
agent_yaml "$W/hub" dparent dparent
agent_yaml "$W/hub" dlost dworker "on_death: { max_auto_fraction: 1.0 }"
agent_yaml "$W/hub" dslow dworker "limits: { wall_clock_secs: 6 }"
sed -i '0,/^limits: { max_sessions: 4 }$/{/^limits: { max_sessions: 4 }$/d}' "$W/hub/agents/dslow.yaml"
join_node worker-b >/dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
echo "checkpoint_every_turns: 1" >> "$W/worker-b/node.yaml"
agent_yaml "$W/worker-b" dhalt dworker "on_death: { max_auto_cost_usd: 0.0 }"
agent_yaml "$W/worker-b" dlost dworker "on_death: { max_auto_fraction: 1.0 }"
start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b
wait_for 20 "agents loaded" bash -c "$O agents --json | python3 -c 'import sys,json;n={(a[\"node\"],a[\"agent\"][\"name\"]) for a in json.load(sys.stdin)};sys.exit(0 if {(\"hub\",\"dauto\"),(\"hub\",\"dslow\"),(\"worker-b\",\"dlost\")}<=n else 1)'"
HWS="$W/hub/data/workspaces"
# kill_oc ID: kill -9 the session's OpenCode (the bridge then exits 137, like a container).
kill_oc() {
  local b
  b=$(pgrep -f "unix:[^ ]*$1/oc.sock" | head -1)
  [ -n "$b" ] && pkill -9 -P "$b"
}
in_sleep() { $O events --session "$1" --limit 2000 --json | grep -c '"tool.called"' | python3 -c "import sys;sys.exit(0 if int(sys.stdin.read())>=${2:-2} else 1)"; }
export -f in_sleep

say "killed mid-step: detected, restarted in place, history intact"
S1=$(spawn_id dauto -p "work" --node hub)
wait_for 150 "S1 is in its long step" bash -c "[ -f $HWS/$S1/a.txt ] && in_sleep $S1 2"
sleep 2; kill_oc "$S1"
wait_for 20 "the death is detected as exit:137" bash -c "$O events --session $S1 --limit 2000 | grep -q 'session.dead.*exit:137'"
wait_for 20 "recovered automatically by a restart" bash -c "$O events --session $S1 --limit 2000 | grep -q 'recovery.auto.*restart'"
wait_for 150 "it continued after the restart and finished" bash -c "[ \"\$(sess 's[\"last_text\"]' $S1)\" = 'recovered and finished' ] && [ \"\$(sess_state $S1)\" = idle ]"
expect "same OpenCode session: the history before the crash is kept" bash -c "[ \"\$(sess 's[\"turns\"]' $S1)\" = 1 ] && [ -f $HWS/$S1/a.txt ] && [ \"\$(cat $HWS/$S1/b.txt)\" = 'after restart' ]"
expect "deaths recovered: 1" bash -c "[ \"\$(sess 's[\"deaths\"]' $S1)\" = 1 ]"
$O events --session "$S1" --limit 2000 | grep -E 'session.dead|recovery' | sed 's/^/  /'

say "when recovery is not cheap enough: the tree is halted"
PA=$(spawn_id dparent --coordinator -p "start" --node hub)
wait_for 150 "the parent's child is in its long step" bash -c "$O sessions --json | python3 -c 'import sys,json;sys.exit(0 if any(s[\"parent\"]==\"$PA\" for s in json.load(sys.stdin)) else 1)'"
CH=$($O sessions --json | python3 -c "import sys,json;print([s['id'] for s in json.load(sys.stdin) if s['parent']=='$PA'][0])")
wait_for 150 "child working" bash -c "[ -f $HWS/$CH/a.txt ] && in_sleep $CH 2"
sleep 2; kill_oc "$CH"
wait_for 30 "a halt is open for the tree" bash -c "$O halts --json | python3 -c 'import sys,json;sys.exit(0 if any(h[\"dead_session\"]==\"$CH\" for h in json.load(sys.stdin)) else 1)'"
$O halts | sed 's/^/  /'
H=$($O halts --json | python3 -c "import sys,json;print([h['id'] for h in json.load(sys.stdin) if h['dead_session']=='$CH'][0])")
expect "scope is the whole tree, from the parent" bash -c "$O halts --json | grep -q '\"scope\": \"session:$PA\"'"
expect "the parent is halted (container kept)" bash -c "[ \"\$(sess_state $PA)\" = halted ]"
expect "the child is dead with its reason" bash -c "[ \"\$(sess 's[\"state\"]+\"/\"+s[\"dead_reason\"]' $CH)\" = dead/exit:137 ]"
expect "options are priced; restart and resume are available" bash -c "$O halts --json | python3 -c 'import sys,json;h=[h for h in json.load(sys.stdin) if h[\"id\"]==\"$H\"][0];o={x[\"kind\"]:x for x in h[\"options\"]};sys.exit(0 if o[\"restart\"][\"available\"] and o[\"restart\"][\"est_cost_usd\"]>0 and o[\"resume_elsewhere\"][\"available\"] and o[\"handoff\"][\"available\"] and h[\"current_todo\"]==\"write a\" else 1)'"
expect "why it was not automatic is in the notification" bash -c "$O notifications --json | grep -q 'above max_auto_cost_usd'"
$O recover "$H" restart >/dev/null
wait_for 150 "after 'restart' the child finished" bash -c "[ \"\$(sess 's[\"last_text\"]' $CH)\" = 'recovered and finished' ]"
wait_for 150 "the halted parent resumed" bash -c "[ \"\$(sess_state $PA)\" = idle ] && $O show $PA --json | grep -q 'parent resumed\\|noted'"
expect "the halt is resolved" bash -c "$O halts --all --json | python3 -c 'import sys,json;sys.exit(0 if [h for h in json.load(sys.stdin) if h[\"id\"]==\"$H\"][0][\"status\"]==\"resolved\" else 1)'"

say "resume on another machine from the dead instance's volume"
S3=$(spawn_id dhalt -p "work" --node hub)
wait_for 150 "S3 working" bash -c "[ -f $HWS/$S3/a.txt ] && in_sleep $S3 2"
sleep 2; kill_oc "$S3"
wait_for 30 "halted" bash -c "$O halts --json | grep -q '\"dead_session\": \"$S3\"'"
H3=$($O halts --json | python3 -c "import sys,json;print([h['id'] for h in json.load(sys.stdin) if h['dead_session']=='$S3'][0])")
NEW=$($O recover "$H3" resume_elsewhere --node worker-b | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["replacement"])')
[ -n "$NEW" ] && pass "replacement $NEW started" || fail "replacement started"
wait_for 150 "the replacement on worker-b finished" bash -c "[ \"\$(sess 's[\"last_text\"]' $NEW)\" = 'replacement finished' ]"
expect "it runs on worker-b and replaces the dead one" bash -c "[ \"\$(sess 's[\"node\"]+\"/\"+s[\"replaces\"]' $NEW)\" = worker-b/$S3 ]"
expect "its workspace started from the dead instance's files" bash -c "[ \"\$(cat $W/worker-b/data/workspaces/$NEW/a.txt)\" = 'before the crash' ] && [ -f $W/worker-b/data/workspaces/$NEW/c.txt ]"
expect "it was briefed with the original prompt and todos" bash -c "$O show $NEW --json | python3 -c 'import sys,json;p=json.load(sys.stdin)[\"prompt\"];sys.exit(0 if \"Its original instructions:\\nwork\" in p and \"write a\" in p else 1)'"
wait_for 30 "the dead original is stopped (it was harvested when it died)" bash -c "[ \"\$(sess_state $S3)\" = stopped ] && $O harvests --session $S3 --json | grep -q '\"trigger\": \"death\"'"

say "a machine is lost: resume from its latest checkpoint"
SL=$(spawn_id dlost -p "one turn" --node worker-b)
wait_for 150 "turn 1 done on worker-b" bash -c "[ \"\$(sess 's[\"last_text\"]' $SL)\" = 'turn 1 done' ]"
wait_for 30 "a checkpoint was taken after the turn" bash -c "$O events --session $SL --limit 500 | grep -q checkpoint"
$O send "$SL" "continue" >/dev/null
wait_for 30 "SL in its long step" bash -c "[ \"\$(sess_state $SL)\" = running ]"
sleep 4
kill "$PID_worker_b"
for b in $(pgrep -f "unix:$(run_dir worker-b)/"); do pkill -9 -P "$b"; kill -9 "$b"; done
wait_for 40 "lost after the grace period, as a death (node_lost)" bash -c "$O events --session $SL --limit 2000 | grep -q 'session.dead.*node_lost'"
wait_for 30 "resumed elsewhere automatically" bash -c "$O events --session $SL --limit 2000 | grep -q 'recovery.auto.*resume_elsewhere'"
NL=$($O sessions --json | python3 -c "import sys,json;print([s['id'] for s in json.load(sys.stdin) if s.get('replaces')=='$SL'][0])")
wait_for 150 "the replacement on the hub finished" bash -c "[ \"\$(sess 's[\"last_text\"]' $NL)\" = 'replacement finished' ]"
expect "its workspace came from the checkpoint" bash -c "[ \"\$(cat $HWS/$NL/a.txt)\" = 'before the crash' ]"
expect "the harvest says outputs after the checkpoint are unrecoverable" bash -c "$O harvests --session $SL --json | python3 -c 'import sys,json;h=json.load(sys.stdin)[0];sys.exit(0 if h[\"from_checkpoint\"] and \"unrecoverable\" in h[\"notes\"][0] else 1)'"

say "the wall-clock limit ends an instance as dead (no automatic retry)"
SW=$(spawn_id dslow -p "work" --node hub)
wait_for 40 "dead: wall_clock" bash -c "[ \"\$(sess 's[\"state\"]+\"/\"+str(s.get(\"dead_reason\"))' $SW)\" = dead/wall_clock ]"
wait_for 20 "halted for the operator" bash -c "$O halts --json | grep -q '\"dead_session\": \"$SW\"'"
expect "with the reason why it was not retried" bash -c "$O notifications --json | grep -q 'wall-clock limit'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
