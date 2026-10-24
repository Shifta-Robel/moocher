#!/usr/bin/env bash
# Phase 3 e2e: harvesting and cleanup. A template's outputs are collected
# when an instance stops (files, globs as one tree, missing ones reported),
# delivered to the parent's workspace or the task workspace, the transcript
# is kept as an artifact, and volumes are removed according to retain:.
source "$(dirname "$0")/lib.sh"

say "hub with templates that declare outputs"
start_hub
mock_script hworker <<'JSON'
[
  {"tool": "write", "args": {"filePath": "report.md", "content": "# Report\nall good\n"}},
  {"tool": "write", "args": {"filePath": "dist/pkg-1.0-py3-none-any.whl", "content": "wheel 1"}},
  {"tool": "write", "args": {"filePath": "dist/pkg-1.0.tar.gz", "content": "sdist"}},
  {"tool": "write", "args": {"filePath": "dist/extra-2.0-py3-none-any.whl", "content": "wheel 2"}},
  {"text": "built"}
]
JSON
mock_script hparent <<'JSON'
{"phases": [
  {"match": "finished a turn", "steps": [{"tool": "cluster_stop_session", "args": {"session": "{{match:s-[0-9a-f]+}}"}}, {"text": "stopped my child"}]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "hworker", "prompt": "build", "title": "builder"}}, {"text": "spawned"}]}
]}
JSON
mock_script hcoord <<'JSON'
{"phases": [
  {"match": "finished a turn", "steps": [{"tool": "cluster_task_complete", "args": {"summary": "built by the worker"}}, {"text": "task done"}]},
  {"match": "[cluster]", "steps": [{"text": "waiting"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "htask", "prompt": "build", "title": "task worker"}}, {"text": "spawned"}]}
]}
JSON
mock_prices hworker hparent hcoord
OUTPUTS='outputs:
  - { path: report.md, to: parent_workspace, dest: from-child/report.md }
  - { path: "dist/*.whl", to: artifact, name: wheels }
  - { path: missing.txt, to: artifact }
transcript: true'
agent_yaml "$W/hub" hworker hworker "$OUTPUTS
retain: never"
agent_yaml "$W/hub" hkeep hworker "outputs: [ { path: report.md } ]"
agent_yaml "$W/hub" hparent hparent
agent_yaml "$W/hub" htask hworker "outputs: [ { path: report.md, to: task_workspace, dest: reports/ } ]"
agent_yaml "$W/hub" coordinator hcoord
WS="$W/hub/data/workspaces"
wait_for 20 "agents loaded" bash -c "$O agents --json | python3 -c 'import sys,json;n={a[\"agent\"][\"name\"] for a in json.load(sys.stdin)};sys.exit(0 if {\"hworker\",\"hparent\",\"htask\",\"hkeep\"}<=n else 1)'"

say "the parent stops its child: outputs are collected and delivered"
P=$(spawn_id hparent --coordinator -p "start")
wait_for 150 "child spawned" bash -c "$O sessions --json | python3 -c 'import sys,json;sys.exit(0 if any(s[\"parent\"]==\"$P\" for s in json.load(sys.stdin)) else 1)'"
C=$($O sessions --json | python3 -c "import sys,json;print([s['id'] for s in json.load(sys.stdin) if s['parent']=='$P'][0])")
wait_for 150 "child stopped by its parent" bash -c "[ \"\$(sess_state $C)\" = stopped ]"
wait_for 30 "harvest recorded" bash -c "[ -n \"\$($O harvests --session $C --json | python3 -c 'import sys,json;print(len(json.load(sys.stdin)) or \"\")')\" ]"
$O harvests --session "$C" | sed 's/^/  /'
H() { $O harvests --session "$C" --json | python3 -c "import sys,json;h=json.load(sys.stdin)[0];o={x['pattern']:x for x in h['outputs']};print($1)"; }
expect "report.md delivered into the parent's workspace at dest" bash -c "[ \"\$(head -1 $WS/$P/from-child/report.md)\" = '# Report' ]"
expect "the two wheels (not the sdist) became one tree artifact" bash -c "R=\$($O harvests --session $C --json | python3 -c 'import sys,json;print([x for x in json.load(sys.stdin)[0][\"outputs\"] if x[\"pattern\"]==\"dist/*.whl\"][0][\"reference\"])') && $O artifact get \$R -o $W/wheels >/dev/null && ls $W/wheels/dist | tr '\\n' ' ' | grep -qx 'extra-2.0-py3-none-any.whl pkg-1.0-py3-none-any.whl '"
expect "a missing output is reported, not an error" bash -c "$O harvests --session $C --json | python3 -c 'import sys,json;sys.exit(0 if json.load(sys.stdin)[0][\"missing\"]==[\"missing.txt\"] else 1)'"
expect "the transcript is an artifact with the conversation" bash -c "T=\$($O harvests --session $C --json | python3 -c 'import sys,json;print(json.load(sys.stdin)[0][\"transcript_ref\"])') && $O artifact get \$T -o $W/tr.json >/dev/null && python3 -c 'import json;d=json.load(open(\"$W/tr.json\"));assert d[\"session\"]==\"$C\" and len(d[\"messages\"])>=2'"
expect "trigger recorded (stop)" bash -c "$O harvests --session $C --json | grep -q '\"trigger\": \"stop\"'"
expect "retain: never removed the volumes at once" bash -c "[ ! -e $WS/$C ] && [ ! -e $W/hub/data/instances/$C ]"
expect "artifacts carry their producer" bash -c "$O artifact ls wheels --json | grep -q '\"producer\": \"$C\"'"

say "default retention keeps the volumes for 24 h"
K=$(spawn_id hkeep -p "build")
wait_for 150 "hkeep finished" bash -c "[ \"\$(sess_state $K)\" = idle ]"
$O stop "$K" >/dev/null
expect "workspace and home kept after stop" bash -c "[ -f $WS/$K/report.md ] && [ -d $W/hub/data/instances/$K/home ]"
expect "report.md kept as an artifact (default destination)" bash -c "$O harvests --session $K --json | grep -q '\"to\": \"artifact\"'"

say "a task ends: its sessions are harvested into the task workspace"
TID=$($O task add "build something" --label on_demand --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
$O task run "$TID" >/dev/null
wait_for 120 "task done" bash -c "$O task show $TID | python3 -c 'import sys,json;sys.exit(0 if json.load(sys.stdin)[\"status\"]==\"done\" else 1)'"
TW=$($O sessions --all --json | python3 -c "import sys,json;print([s['id'] for s in json.load(sys.stdin) if s['agent']=='htask' and s.get('task_id')==$TID][0])")
wait_for 30 "the worker's report is in the task workspace" bash -c "[ -f $WS/shared-task-$TID/reports/report.md ]"
expect "harvest trigger task_end" bash -c "$O harvests --session $TW --json | grep -q '\"trigger\": \"task_end\"'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
