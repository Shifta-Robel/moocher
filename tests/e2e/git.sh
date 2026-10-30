#!/usr/bin/env bash
# Git in workspaces: one commit identity from the hub, a note on every commit
# saying which agent made it, a forked workspace on its own branch whose
# history is handed back to the parent's repository, and a shared repository
# kept intact across two machines (history travels as bundles, .git is never
# merged file by file).
source "$(dirname "$0")/lib.sh"

say "hub (git identity set) + worker-b"
ENROLL_YAML="git: { name: Moo Tester, email: tester@example.org }" start_hub
mock_script gparent <<'JSON'
{"phases": [
  {"match": "handed back", "steps": [{"text": "got the work back"}]},
  {"match": "finished a turn", "steps": [{"tool": "cluster_stop_session", "args": {"session": "{{match:s-[0-9a-f]+}}"}}, {"text": "stopped my child"}]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [
    {"tool": "bash", "args": {"command": "git init -q -b main && printf 'v1\\n' > app.py && git add -A && git commit -qm 'parent: v1'", "description": "start a repo"}},
    {"tool": "cluster_spawn_agent", "args": {"agent": "gchild", "prompt": "improve app.py", "title": "child", "fork_workspace": true}},
    {"text": "spawned"}]}
]}
JSON
mock_script gchild <<'JSON'
[
  {"tool": "bash", "args": {"command": "printf 'v2\\n' >> app.py && git commit -qam 'child: v2'", "description": "improve"}},
  {"text": "improved"}
]
JSON
mock_script gsharea <<'JSON'
{"phases": [
  {"match": "again", "steps": [{"tool": "bash", "args": {"command": "printf 'a2\\n' >> a.txt && git commit -qam 'A: second'", "description": "commit"}}, {"text": "a2"}]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "bash", "args": {"command": "git init -q -b main && printf 'a1\\n' > a.txt && git add -A && git commit -qm 'A: first'", "description": "commit"}}, {"text": "a1"}]}
]}
JSON
mock_script gshareb <<'JSON'
{"phases": [
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "bash", "args": {"command": "printf 'b1\\n' > b.txt && git add b.txt && git commit -qm 'B: first'", "description": "commit"}}, {"text": "b1"}]}
]}
JSON
mock_prices gparent gchild gsharea gshareb
git_agent() { # git_agent DIR NAME [extra yaml]
  mkdir -p "$1/agents"
  cat > "$1/agents/$2.yaml" <<EOF
name: $2
description: e2e git agent $2
provider: { id: mock, model: $2, key: { ref: "env:MOCK_API_KEY" } }
tools: { edit: allow, bash: { default: allow } }
limits: { max_sessions: 4 }
${3:-}
EOF
}
git_agent "$W/hub" gparent
git_agent "$W/hub" gchild "outputs: [ { git: true, to: parent_workspace } ]"
git_agent "$W/hub" gsharea
join_node worker-b >/dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
git_agent "$W/worker-b" gshareb
start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b
wait_for 20 "agents loaded" bash -c "$O agents --json | python3 -c 'import sys,json;n={(a[\"node\"],a[\"agent\"][\"name\"]) for a in json.load(sys.stdin)};sys.exit(0 if {(\"hub\",\"gparent\"),(\"hub\",\"gchild\"),(\"worker-b\",\"gshareb\")}<=n else 1)'"
WS="$W/hub/data/workspaces"
G() { git -C "$1" "${@:2}"; }

say "fork: the child starts from the parent's repository on its own branch"
P=$(spawn_id gparent --coordinator -p "start")
wait_for 120 "child spawned" bash -c "$O sessions --all --json | python3 -c 'import sys,json;sys.exit(0 if any(s[\"parent\"]==\"$P\" for s in json.load(sys.stdin)) else 1)'"
C=$($O sessions --all --json | python3 -c "import sys,json;print([s['id'] for s in json.load(sys.stdin) if s['parent']=='$P'][0])")
wait_for 120 "child stopped by its parent" bash -c "[ \"\$(sess_state $C)\" = stopped ]"
expect "the child's copy is on branch agent/gchild/$C" bash -c "[ \"\$(git -C $WS/$C rev-parse --abbrev-ref HEAD)\" = agent/gchild/$C ]"
expect "it has the parent's commit underneath its own" bash -c "[ \"\$(git -C $WS/$C log --format=%s | tr '\\n' '|')\" = 'child: v2|parent: v1|' ]"
expect "no bundle left lying in the child's workspace" bash -c "[ ! -e $WS/$C/.moo-git ]"
expect "the fork snapshot carries a bundle, not .git" bash -c "R=\$($O artifact ls fork --json | python3 -c 'import sys,json;print(json.load(sys.stdin)[0][\"reference\"])') && $O artifact get \$R -o $W/forksnap >/dev/null && [ -f $W/forksnap/.moo-git/bundle ] && [ ! -e $W/forksnap/.git ] && [ -f $W/forksnap/app.py ]"

say "hand-back: the child's history lands in the parent's repository"
wait_for 60 "parent's main fast-forwarded to the child's commit" bash -c "[ \"\$(git -C $WS/$P log -1 --format=%s)\" = 'child: v2' ]"
expect "and its working tree has the change" bash -c "[ \"\$(tail -1 $WS/$P/app.py)\" = v2 ]"
expect "the child's branch is a remote-tracking ref named after it" bash -c "git -C $WS/$P rev-parse -q --verify refs/remotes/$C/agent/gchild/$C >/dev/null"
expect "the parent's working tree is clean" bash -c "[ -z \"\$(git -C $WS/$P status --porcelain)\" ]"
expect "every commit is by the configured identity" bash -c "[ \"\$(git -C $WS/$P log --format='%an <%ae>|%cn' | sort -u)\" = 'Moo Tester <tester@example.org>|Moo Tester' ]"
expect "the child's commit has a note naming it" bash -c "git -C $WS/$P notes --ref=moo show HEAD | grep -qx 'moocher-agent: gchild' && git -C $WS/$P notes --ref=moo show HEAD | grep -qx 'moocher-session: $C' && git -C $WS/$P notes --ref=moo show HEAD | grep -qx 'moocher-parent: $P'"
expect "the parent's own commit has the parent's note" bash -c "git -C $WS/$P notes --ref=moo show HEAD~1 | grep -qx 'moocher-agent: gparent'"
expect "the commit messages stay clean (no trailers)" bash -c "! git -C $WS/$P log --format=%B | grep -q moocher"
expect "harvest recorded the git output as delivered" bash -c "$O harvests --session $C --json | python3 -c 'import sys,json;o=json.load(sys.stdin)[0][\"outputs\"][0];assert o[\"kind\"]==\"git\" and \"fast-forward\" in o[\"delivered\"], o'"
wait_for 60 "the parent was told (and answered)" bash -c "[ \"\$(sess 's[\"last_text\"]' $P)\" = 'got the work back' ]"

say "a shared repository on two machines"
SA=$(spawn_id gsharea -p "start" --node hub --shared proj)
wait_for 60 "A committed on the hub" bash -c "git -C $WS/shared-proj log --format=%s 2>/dev/null | grep -qx 'A: first'"
SB=$(spawn_id gshareb -p "go" --node worker-b --shared proj)
HUB="$WS/shared-proj"; WB="$W/worker-b/data/workspaces/shared-proj"
wait_for 60 "worker-b's copy is a real repository with A's history" bash -c "git -C $WB log --format=%s 2>/dev/null | grep -qx 'A: first'"
wait_for 60 "B's commit sits on top of A's on worker-b" bash -c "[ \"\$(git -C $WB log --format=%s | tr '\\n' '|')\" = 'B: first|A: first|' ]"
wait_for 60 "B's commit reached the hub's repository" bash -c "[ \"\$(git -C $HUB log -1 --format=%s 2>/dev/null)\" = 'B: first' ]"
expect "the hub's copy is clean (index moved with the branch)" bash -c "[ -z \"\$(git -C $HUB status --porcelain)\" ]"
expect "B's note came along" bash -c "git -C $HUB notes --ref=moo show HEAD | grep -qx 'moocher-agent: gshareb'"
expect "git fsck is happy on both machines" bash -c "git -C $HUB fsck --no-progress >/dev/null 2>&1 && git -C $WB fsck --no-progress >/dev/null 2>&1"
$O send "$SA" "again" >/dev/null
wait_for 60 "A's second commit reached worker-b" bash -c "[ \"\$(git -C $WB log -1 --format=%s 2>/dev/null)\" = 'A: second' ]"
expect "worker-b's copy is clean too" bash -c "[ -z \"\$(git -C $WB status --porcelain)\" ]"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
