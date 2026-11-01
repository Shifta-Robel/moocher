#!/usr/bin/env bash
# Key pools: a session moves to another key when one runs out or is refused,
# sessions start on the least used key, right_now work makes others give way
# under pressure, and a node out of keys borrows one, encrypted, from another.
MOCK_ARGS="--ip-quota 3 --ip-window 20 --quota-by-key --accept-key pool-a-secret --accept-key pool-b-secret --accept-key pool-c-secret --accept-key pool-d-secret --accept-key pool-e-secret"
source "$(dirname "$0")/lib.sh"

say "hub with three pools, a worker with none"
start_hub
mkdir -p "$W/k"
for k in a b c d e; do echo "pool-$k-secret" > "$W/k/$k"; done
echo pool-bad-secret > "$W/k/bad"
pool_providers() { cat <<YAML
    quota: { scope: key }
    keys:
      - { ref: "file:$W/k/bad" }
      - { ref: "file:$W/k/a" }
      - { ref: "file:$W/k/b" }
  mockc:
    base_url: http://127.0.0.1:8099/v1
    api: openai
    quota: { scope: key }
    keys:
      - { ref: "file:$W/k/c" }
  mockd:
    base_url: http://127.0.0.1:8099/v1
    api: openai
    quota: { scope: key }
    keys:
      - { ref: "file:$W/k/d", lend: true }
  mocke:
    base_url: http://127.0.0.1:8099/v1
    api: openai
    quota: { scope: key }
    keys:
      - { ref: "file:$W/k/e" }
YAML
}
cp "$W/hub/providers.yaml" "$W/providers.base"
pool_providers >> "$W/hub/providers.yaml"
pooled() { # pooled DIR NAME PROVIDER MODEL
  cat > "$1/agents/$2.yaml" <<YAML
name: $2
description: e2e agent $2
provider: { id: $3, model: $4 }
tools: { edit: allow, bash: { default: ask } }
limits: { max_sessions: 4 }
YAML
}
call='{"tool": "cluster_read_inbox", "args": {}}'
mock_script swapm <<JSON
[$call, $call, $call, $call, {"text": "swap done"}]
JSON
mock_script firstm <<'JSON'
[{"text": "first here"}]
JSON
mock_script secondm <<'JSON'
[{"text": "second here"}]
JSON
mock_script urgentm <<JSON
{"phases": [{"match": "again", "steps": [{"text": "urgent again"}]}, {"steps": [$call, $call, $call, $call, {"text": "urgent done"}]}]}
JSON
mock_script normalm <<'JSON'
{"phases": [{"match": "later", "steps": [{"text": "normal resumed"}]}, {"steps": [{"text": "normal here"}]}]}
JSON
mock_script borrowm <<'JSON'
[{"text": "borrowed fine"}]
JSON
mock_script nolendm <<'JSON'
[{"text": "should not answer"}]
JSON
mock_prices swapm firstm secondm urgentm normalm borrowm nolendm
pooled "$W/hub" swap mock swapm
pooled "$W/hub" first mock firstm
pooled "$W/hub" second mock secondm
pooled "$W/hub" urgent mockc urgentm
pooled "$W/hub" normal mockc normalm
pooled "$W/hub" lender-only mockd borrowm
wait_for 30 "agents loaded" bash -c "$O agents | grep -q lender-only"
auths() { curl -s http://127.0.0.1:8099/v1/_log | python3 -c "import sys;import json;print(' '.join(e['auth'].split()[-1] for e in json.load(sys.stdin) if e['model']=='$1'))"; }
export -f auths
pool_json() { $O pool --json; }

say "a session moves to the next key when its own runs out, and a refused key is skipped"
SW=$(spawn_id swap -p "go")
wait_for 90 "it finishes across keys" bash -c "[ \"\$(sess 's[\"last_text\"]' $SW)\" = 'swap done' ]"
A=$(auths swapm); echo "  keys used: $A"
expect "the provider refused one key, and the turn went on" bash -c "echo '$A' | grep -q pool-bad-secret"
expect "it stayed on one key until it ran out" bash -c "[ \"\$(echo '$A' | tr ' ' '\n' | grep -v pool-bad | head -3 | sort -u | wc -l)\" = 1 ]"
expect "then used a second one" bash -c "[ \"\$(echo '$A' | tr ' ' '\n' | grep -v pool-bad | sort -u | wc -l)\" = 2 ]"
expect "the event says so" bash -c "$O events --session $SW --limit 100 | grep -q key.switched"
expect "the refusal alerts the operator" bash -c "$O events --session $SW --limit 100 | grep -q provider.auth_failed"
wait_for 20 "moo pool lists the keys" bash -c "$O pool | grep -q 'refused by the provider'"
expect "and which session uses what" bash -c "$O pool | grep -q $SW"
expect_fail "and never a key" bash -c "$O pool --json | grep -q secret"
stop_all

say "sessions start on the least used key"
sleep 21   # the window passes
F=$(spawn_id first -p "go")
wait_for 60 "the first session answers" bash -c "[ \"\$(sess 's[\"last_text\"]' $F)\" = 'first here' ]"
wait_for 20 "the hub knows it is in use" bash -c "$O pool | grep -q $F"
N=$(spawn_id second -p "go")
wait_for 60 "the second answers" bash -c "[ \"\$(sess 's[\"last_text\"]' $N)\" = 'second here' ]"
expect "they are on different keys" bash -c "[ \"\$(auths firstm)\" != \"\$(auths secondm)\" ]"
stop_all

say "right_now hitting a limit makes less urgent work give way"
N=$(spawn_id normal -p "go")
wait_for 60 "normal work runs on the key" bash -c "[ \"\$(sess 's[\"last_text\"]' $N)\" = 'normal here' ]"
R=$(spawn_id urgent -p "go" --urgency right_now)
wait_for 60 "right_now work hits the limit" bash -c "$O events --session $R --limit 100 | grep -q quota.yield"
wait_for 20 "moo pool shows the key kept for it" bash -c "$O pool | grep -A3 'KEPT' | grep -q mockc"
wait_for 90 "right_now work finishes" bash -c "[ \"\$(sess 's[\"last_text\"]' $R)\" = 'urgent done' ]"
expect_fail "normal work has not been stopped" bash -c "[ \"\$(sess_state $N)\" = stopped ]"
sleep 21   # the allowance is back, for everyone but it
$O send "$N" "later" >/dev/null
wait_for 30 "its next call waits" bash -c "[ \"\$(sess_state $N)\" = parked ] || $O events --session $N --limit 50 | grep -q 'kept for'"
expect_fail "it has not answered" bash -c "[ \"\$(sess 's[\"last_text\"]' $N)\" = 'normal resumed' ]"
$O send "$R" "again" >/dev/null
wait_for 60 "right_now work still gets through" bash -c "[ \"\$(sess 's[\"last_text\"]' $R)\" = 'urgent again' ]"
$O stop "$R" >/dev/null
wait_for 120 "normal work carries on when it is done" bash -c "[ \"\$(sess 's[\"last_text\"]' $N)\" = 'normal resumed' ]"
stop_all

say "a node out of keys borrows from another, which must have agreed"
join_node worker-b > /dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
rm -f "$W/worker-b/agents/"*
cp "$W/providers.base" "$W/worker-b/providers.yaml"
cat >> "$W/worker-b/providers.yaml" <<YAML
    quota: { scope: key }
  mockd:
    base_url: http://127.0.0.1:8099/v1
    api: openai
    quota: { scope: key }
    borrow: true
  mocke:
    base_url: http://127.0.0.1:8099/v1
    api: openai
    quota: { scope: key }
    borrow: true
YAML
pooled "$W/worker-b" borrower mockd borrowm
pooled "$W/worker-b" nolend mocke nolendm
start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b
wait_for 30 "the hub offers a lendable key" bash -c "$O pool --json | python3 -c 'import sys,json;sys.exit(0 if any(k[\"lendable\"] and k[\"provider\"]==\"mockd\" for k in json.load(sys.stdin)[\"keys\"]) else 1)'"
B=$(spawn_id borrower -p "go" --node worker-b)
wait_for 60 "the worker answers with the borrowed key" bash -c "[ \"\$(sess 's[\"last_text\"]' $B)\" = 'borrowed fine' ]"
expect "it was the lender's key" bash -c "[ \"\$(auths borrowm)\" = pool-d-secret ]"
expect "the loan is audited" bash -c "$O audit --limit 50 | grep -q key.lent"
wait_for 20 "moo pool shows it as borrowed" bash -c "$O pool | grep worker-b | grep -q borrowed"
expect_fail "the key is nowhere in the hub's data" grep -rqa pool-d-secret "$W/hub/data" "$W/hub/supervisor.db" "$W/supervisor.log"
expect_fail "or on the worker's disk" grep -rqa pool-d-secret "$W/worker-b" "$W/node-worker-b.log"
X=$(spawn_id nolend -p "go" --node worker-b)
sleep 15
expect "a key not marked lend: true stays home" bash -c "[ -z \"\$(auths nolendm)\" ]"
stop_all

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
