#!/usr/bin/env bash
# Phase 3 e2e: the artifact store (operator and agents), lineage, shared
# workspaces across two machines (sync, three-way merge, conflicts) and a
# workflow whose steps run on different machines.
source "$(dirname "$0")/lib.sh"

say "hub + worker-b"
start_hub
mock_script sharer <<'JSON'
{"phases": [
  {"match": "Workspace sync", "steps": [{"text": "noted the conflict"}]},
  {"match": "update", "steps": [
    {"tool": "write", "args": {"filePath": "shared.txt", "content": "from A, second turn"}},
    {"text": "updated"}]},
  {"steps": [
    {"tool": "write", "args": {"filePath": "greet.py", "content": "def greet(n):\n    return f'hello {n}'\n"}},
    {"tool": "write", "args": {"filePath": "pkg/a.txt", "content": "A"}},
    {"tool": "write", "args": {"filePath": "pkg/sub/b.txt", "content": "B"}},
    {"tool": "cluster_artifact_put", "args": {"path": "greet.py", "tags": ["e2e"]}},
    {"tool": "cluster_artifact_put", "args": {"path": "pkg", "name": "pkg-tree"}},
    {"tool": "write", "args": {"filePath": "shared.txt", "content": "from A"}},
    {"text": "shared both"}]}
]}
JSON
mock_script fetcher <<'JSON'
[
  {"tool": "cluster_artifact_get", "args": {"ref": "{{last_user}}", "path": "in/greet.py"}},
  {"tool": "write", "args": {"filePath": "in/derived.py", "content": "from greet import greet\nprint(greet('x'))\n"}},
  {"tool": "cluster_artifact_put", "args": {"path": "in/derived.py", "name": "derived.py"}},
  {"text": "derived"}
]
JSON
mock_script syncer <<'JSON'
{"phases": [
  {"match": "conflict-1", "steps": [{"tool": "write", "args": {"filePath": "conflict.txt", "content": "B1"}}, {"text": "c1"}]},
  {"match": "conflict-2", "steps": [{"tool": "write", "args": {"filePath": "conflict.txt", "content": "B2"}}, {"text": "c2"}]},
  {"steps": [{"tool": "write", "args": {"filePath": "fromB.txt", "content": "B was here"}}, {"text": "wrote fromB"}]}
]}
JSON
mock_prices sharer fetcher syncer
for a in sharer fetcher syncer; do agent_yaml "$W/hub" "$a" "$a"; done
join_node worker-b >/dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
for a in fetcher syncer; do agent_yaml "$W/worker-b" "$a" "$a"; done
start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b
wait_for 20 "agents loaded" bash -c "$O agents --json | python3 -c 'import sys,json;n={(a[\"node\"],a[\"agent\"][\"name\"]) for a in json.load(sys.stdin)};sys.exit(0 if (\"hub\",\"sharer\") in n and (\"worker-b\",\"syncer\") in n else 1)'"

say "operator: put and get a file and a tree"
mkdir -p "$W/up/tree/deep"
printf 'hello artifact\n' > "$W/up/note.txt"
printf 'one' > "$W/up/tree/one.txt"; printf 'two' > "$W/up/tree/deep/two.txt"; chmod +x "$W/up/tree/deep/two.txt"
ln -s /etc/passwd "$W/up/tree/link-out"
F=$($O artifact put "$W/up/note.txt" --tag e2e --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["reference"])')
T=$($O artifact put "$W/up/tree" --name uptree --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["reference"])')
[ -n "$F" ] && [ -n "$T" ] && pass "uploaded a file ($F) and a tree ($T)" || fail "uploaded a file and a tree"
expect "same content, same reference (content-addressed)" bash -c "[ \"\$($O artifact put '$W/up/note.txt' --json | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"reference\"])')\" = '$F' ]"
(cd "$W" && $O artifact get "$F" -o "$W/got-note.txt" >/dev/null)
expect "file comes back identical" cmp "$W/up/note.txt" "$W/got-note.txt"
$O artifact get "$T" -o "$W/got-tree" >/dev/null
expect "tree comes back identical (content and exec bit)" bash -c "diff -r --exclude link-out '$W/up/tree' '$W/got-tree' && [ -x '$W/got-tree/deep/two.txt' ]"
expect "links are not packed" test ! -e "$W/got-tree/link-out"
$O artifact ls | head -4 | sed 's/^/  /'

say "agents share artifacts; lineage follows what they fetched"
A=$(spawn_id sharer -p "share your work" --node hub)
wait_for 90 "sharer finished" bash -c "[ \"\$(sess_state $A)\" = idle ]"
GREET=$($O artifact ls --json | python3 -c "import sys,json;a=[x for x in json.load(sys.stdin) if x['name']=='greet.py' and x['producer']=='$A'];print(a[0]['reference'] if a else '')")
TREE=$($O artifact ls --json | python3 -c "import sys,json;a=[x for x in json.load(sys.stdin) if x['name']=='pkg-tree'];print(a[0]['reference'] if a else '')")
[ -n "$GREET" ] && pass "greet.py published by $A ($GREET)" || fail "greet.py published by the agent"
expect "pkg/ published as a tree with both files" bash -c "$O artifact get '$TREE' -o '$W/pkg' >/dev/null && [ \"\$(cat '$W/pkg/pkg/a.txt' '$W/pkg/pkg/sub/b.txt')\" = AB ]"
FB=$(spawn_id fetcher -p "$GREET" --node worker-b)
wait_for 90 "fetcher on worker-b finished" bash -c "[ \"\$(sess_state $FB)\" = idle ]"
expect "the artifact landed in worker-b's workspace" cmp "$W/hub/data/workspaces/$A/greet.py" "$W/worker-b/data/workspaces/$FB/in/greet.py"
DER=$($O artifact ls --json | python3 -c "import sys,json;a=[x for x in json.load(sys.stdin) if x['name']=='derived.py'];print(a[0]['reference'] if a else '')")
expect "derived.py lists greet.py as its parent" bash -c "$O artifact show '$DER' --json | python3 -c 'import sys,json;v=json.load(sys.stdin);sys.exit(0 if \"$GREET\" in v[\"artifact\"][\"parents\"] else 1)'"
expect "greet.py shows derived.py as made from it" bash -c "$O artifact show '$GREET' --json | python3 -c 'import sys,json;v=json.load(sys.stdin);sys.exit(0 if any(c[\"reference\"]==\"$DER\" for c in v[\"children\"]) else 1)'"
$O artifact show "$DER" | sed 's/^/  /'

say "one shared workspace on two machines"
SA=$(spawn_id sharer -p "share your work" --node hub --shared demo)
wait_for 90 "A (hub) finished its turn" bash -c "[ \"\$(sess_state $SA)\" = idle ]"
SB=$(spawn_id syncer -p "go" --node worker-b --shared demo)
HUBWS="$W/hub/data/workspaces/shared-demo"; WBWS="$W/worker-b/data/workspaces/shared-demo"
wait_for 60 "B (worker-b) started with A's files (synced before it started)" bash -c "[ \"\$(cat $WBWS/shared.txt 2>/dev/null)\" = 'from A' ]"
$O show "$SB" | grep placed | sed 's/^/  /'
wait_for 90 "B's new file reached the hub's copy after B's turn" bash -c "[ \"\$(cat $HUBWS/fromB.txt 2>/dev/null)\" = 'B was here' ]"
$O send "$SA" "update shared.txt" >/dev/null
wait_for 90 "A's change reached worker-b after A's turn" bash -c "[ \"\$(cat $WBWS/shared.txt 2>/dev/null)\" = 'from A, second turn' ]"
$O send "$SB" "conflict-1" >/dev/null
wait_for 60 "conflict.txt B1 on the hub" bash -c "[ \"\$(cat $HUBWS/conflict.txt 2>/dev/null)\" = B1 ]"
echo "hub edit" > "$HUBWS/conflict.txt"   # changed here, not synced yet
$O send "$SB" "conflict-2" >/dev/null
wait_for 60 "both sides changed conflict.txt: the hub keeps its version" bash -c "[ -f $HUBWS/conflict.txt.theirs ] && [ \"\$(cat $HUBWS/conflict.txt)\" = 'hub edit' ] && [ \"\$(cat $HUBWS/conflict.txt.theirs)\" = B2 ]"
wait_for 30 "the agent on the hub was told about the conflict" bash -c "[ \"\$(sess 's[\"last_text\"]' $SA)\" = 'noted the conflict' ]"
$O events --limit 400 | grep workspace.synced | tail -3 | sed 's/^/  /'
$O stop "$SA" >/dev/null; $O stop "$SB" >/dev/null

say "a workflow whose review step runs on another machine"
mkdir -p "$W/hub/workflows"
cat > "$W/hub/workflows/xbuild.yaml" <<'YAML'
name: xbuild
description: build on the hub, review on worker-b
steps:
  - name: build
    agent: builder
    node: hub
    prompt: "{{input}}"
    success: { todos: ["write implementation", "write tests", "run tests"] }
  - name: review
    agent: reviewer
    node: worker-b
    prompt: "Review greet.py and test_greet.py. {{prev.output}}"
    success:
      checks: [ { exit_code: "python3 -m pytest -q" }, { text_contains: "VERDICT: APPROVED" } ]
YAML
RUN=$($O workflow run xbuild "Write greet(name) in greet.py with pytest tests in test_greet.py" --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
wait_for 240 "run $RUN passed" bash -c "$O workflow show $RUN | python3 -c 'import sys,json;sys.exit(0 if json.load(sys.stdin)[\"status\"]==\"passed\" else 1)'"
$O workflow runs | head -3 | sed 's/^/  /'
REV=$($O workflow show "$RUN" | python3 -c 'import sys,json;print(json.load(sys.stdin)["steps"][-1]["session"])')
expect "review ran on worker-b" bash -c "[ \"\$(sess 's[\"node\"]' $REV)\" = worker-b ]"
expect "worker-b's copy of the run workspace has the builder's files" bash -c "ls $W/worker-b/data/workspaces/shared-run-*/test_greet.py"
expect "pytest ran on worker-b and passed (exit_code check)" bash -c "$O workflow show $RUN | grep -q 'exit_code .* -> 0'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
