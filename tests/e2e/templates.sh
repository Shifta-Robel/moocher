#!/usr/bin/env bash
# Phase 2 e2e: hub template library with secret aliases, overridable caps,
# instance limits, the layered preamble, account-scoped usage.
source "$(dirname "$0")/lib.sh"

say "hub with a template library; worker-b maps the template's secret alias"
start_hub
mkdir -p "$W/hub/templates" "$W/hub/preambles"
cat > "$W/hub/templates/py-helper.yaml" <<YAML
name: py-helper
description: Small Python helper from the hub library.
provider: { id: mock, model: builder, key: { secret: mock-key } }
role:
  system_prompt_file: prompts/builder.md
  preamble: "Template note: you are a {{template}} helper; keep answers short."
tools: { edit: allow, bash: { default: ask, allow: ["python3 *", "pytest*", "ls*", "cat *"] } }
instances: { max: 1 }
overridable: { role_append: true, budget_usd: 0.50, routes: [direct] }
YAML
cat > "$W/hub/preambles/cluster.md" <<'MD'
## Cluster preamble (hub)
Instance {{instance.id}} of {{template}} on {{node}}, model {{model}}, answering to {{parent}}.
Budget: {{budget}}. Tools: {{tools}}. Question mode: {{question_mode}}.
MD
join_node worker-b >/dev/null && $O node approve worker-b >/dev/null
make_worker worker-b
python3 -I - "$W/worker-b/node.yaml" <<'PY'
import sys
p = sys.argv[1]
open(p, "a").write('secrets:\n  mock-key: { ref: "env:MOCK_API_KEY", account: demo-account }\n')
PY
printf '    quota: { scope: account }\n' >> "$W/worker-b/providers.yaml"
start_node worker-b
wait_for 20 "worker-b connected" is_connected worker-b

say "template library"
wait_for 20 "py-helper is offered by worker-b (source hub)" bash -c "$O agents --json | python3 -c 'import sys,json;sys.exit(0 if any(a[\"node\"]==\"worker-b\" and a[\"agent\"][\"name\"]==\"py-helper\" and a[\"agent\"][\"source\"]==\"hub\" and a[\"agent\"][\"secret_ok\"] for a in json.load(sys.stdin)) else 1)'"
$O templates | sed 's/^/  /'
wait_for 20 "runnable on worker-b only; hub explains the missing alias" bash -c "$O templates --json | python3 -c 'import sys,json;t=json.load(sys.stdin)[\"templates\"][0];sys.exit(0 if t[\"runnable_on\"]==[\"worker-b\"] and any(b[\"node\"]==\"hub\" and \"alias\" in \" \".join(b[\"problems\"]) for b in t[\"not_runnable\"]) else 1)'"
$O templates | sed 's/^/  /'
expect "worker-b cached the library" test -f "$W/worker-b/data/hub-templates.json"
if grep -q "$MOCK_API_KEY" "$W/worker-b/data/hub-templates.json"; then fail "no secret value in the cached library"; else pass "no secret value in the cached library"; fi

say "overrides are checked against the template"
expect_fail "budget above the template's cap is refused" $O spawn py-helper -p "hi" --budget 5
expect_fail "a route outside overridable.routes is refused" $O spawn py-helper -p "hi" --route via-nowhere
SID=$($O spawn py-helper -p "Write greet(name) in greet.py with pytest tests." --budget 0.4 --append "Use type hints everywhere." --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
[ -n "$SID" ] && pass "spawned with allowed overrides ($SID)" || fail "spawned with allowed overrides"
expect_fail "instances.max: a second live instance is refused" $O spawn py-helper -p "hi"

say "layered preamble"
wait_for 60 "preamble readable" bash -c "$O show $SID --preamble | grep -q 'Cluster preamble (hub)'"
PRE=$($O show "$SID" --preamble)
echo "$PRE" | head -8 | sed 's/^/  | /'
echo "$PRE" | contains "Instance $SID of py-helper on worker-b" && pass "variables rendered (instance, template, node)" || fail "variables rendered"
echo "$PRE" | contains "Budget: \$0.40 for this session" && pass "budget override shown" || fail "budget override shown"
echo "$PRE" | contains "Template note: you are a py-helper helper" && pass "template preamble included" || fail "template preamble included"
echo "$PRE" | contains "Use type hints everywhere." && pass "spawner's role_append included" || fail "spawner's role_append included"
echo "$PRE" | contains "{{" && fail "no unrendered variables" || pass "no unrendered variables"

say "the alias resolves to worker-b's key; usage is grouped by account"
wait_for 120 "py-helper finishes a turn (mock accepts only the real key)" bash -c "$O show $SID --json | python3 -c 'import sys,json;s=json.load(sys.stdin);sys.exit(0 if s[\"state\"]==\"idle\" and s[\"tokens\"][\"input\"]>0 else 1)'"
wait_for 10 "requests counted under account:demo-account" bash -c "$O quotas --json | python3 -c 'import sys,json;sys.exit(0 if any(q[\"scope_id\"]==\"account:demo-account\" and q[\"used\"]>0 for q in json.load(sys.stdin)) else 1)'"
$O quotas mock | sed 's/^/  /'

say "library changes reach the nodes"
sed -i 's/^description: .*/description: Updated on the hub./' "$W/hub/templates/py-helper.yaml"
wait_for 30 "worker-b sees the new description" bash -c "$O agents --json | python3 -c 'import sys,json;sys.exit(0 if any(a[\"node\"]==\"worker-b\" and a[\"agent\"][\"description\"]==\"Updated on the hub.\" for a in json.load(sys.stdin)) else 1)'"

say "a node can refuse hub templates"
echo "hub_templates: false" >> "$W/hub/node.yaml"
wait_for 20 "the hub's own node drops py-helper after hub_templates: false" bash -c "$O agents --json | python3 -c 'import sys,json;sys.exit(1 if any(a[\"node\"]==\"hub\" and a[\"agent\"][\"name\"]==\"py-helper\" for a in json.load(sys.stdin)) else 0)'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
