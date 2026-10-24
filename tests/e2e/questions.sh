#!/usr/bin/env bash
# Phase 3 e2e: questions and escalation. cluster_ask and OpenCode's own
# question tool, routing up the chain, escalation with notes, timeouts, the
# four question modes, rules that raise importance, operator-only questions,
# the assumption log, permission requests and budget stops as questions.
source "$(dirname "$0")/lib.sh"

say "hub with scripted askers"
ENROLL_YAML="questions: { answer_timeout_secs: 6 }" start_hub
mock_script qtop <<'JSON'
{"phases": [
  {"match": "Answer to your question", "steps": [{"tool": "write", "args": {"filePath": "answer.txt", "content": "{{last_user}}"}}, {"text": "got it"}]},
  {"steps": [
    {"tool": "cluster_ask", "args": {"question": "Which port should the service listen on?", "options": ["8080", "9090"], "importance": "normal", "context": "the config has no default"}},
    {"text": "asked"}]}
]}
JSON
mock_script qtool <<'JSON'
[
  {"tool": "question", "args": {"questions": [{"question": "Which database should I use?", "header": "DB", "options": [{"label": "sqlite", "description": "simple"}, {"label": "postgres", "description": "robust"}]}]}},
  {"tool": "write", "args": {"filePath": "db.txt", "content": "chosen"}},
  {"text": "done"}
]
JSON
mock_script qparent <<'JSON'
{"phases": [
  {"match": "Nobody above you will answer", "steps": [{"tool": "cluster_answer", "args": {"id": "{{match:q-[0-9a-f]+}}", "answer": "use 9090 (my decision)"}}, {"text": "decided for child"}]},
  {"match": "importance low", "steps": [{"tool": "cluster_answer", "args": {"id": "{{match:q-[0-9a-f]+}}", "answer": "8080 from parent"}}, {"text": "answered child"}]},
  {"match": "importance normal", "steps": [{"tool": "cluster_escalate", "args": {"id": "{{match:q-[0-9a-f]+}}", "note": "parent does not know the deployment port"}}, {"text": "escalated"}]},
  {"match": "importance vital", "steps": [{"tool": "cluster_escalate", "args": {"id": "{{match:q-[0-9a-f]+}}", "note": "too risky for me"}}, {"text": "escalated vital"}]},
  {"match": "[cluster]", "steps": [{"text": "noted"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "qchild", "prompt": "{{last_user}}", "title": "asker"}}, {"text": "spawned child"}]}
]}
JSON
mock_script qlazy <<'JSON'
{"phases": [
  {"match": "[cluster]", "steps": [{"text": "not now"}]},
  {"steps": [{"tool": "cluster_spawn_agent", "args": {"agent": "qchild", "prompt": "{{last_user}}", "title": "asker"}}, {"text": "spawned child"}]}
]}
JSON
mock_script qchild <<'JSON'
{"phases": [
  {"match": "Answer to your question", "steps": [{"tool": "write", "args": {"filePath": "answer.txt", "content": "{{last_user}}"}}, {"text": "child got answer"}]},
  {"match": "About your question", "steps": [{"tool": "write", "args": {"filePath": "answer.txt", "content": "{{last_user}}"}}, {"text": "child decides"}]},
  {"match": "ask-low", "steps": [{"tool": "cluster_ask", "args": {"question": "Tabs or spaces for the config file?", "importance": "low"}}, {"text": "asked low"}]},
  {"match": "ask-normal", "steps": [{"tool": "cluster_ask", "args": {"question": "Which port should the service listen on?", "importance": "normal"}}, {"text": "asked normal"}]},
  {"match": "ask-delete", "steps": [{"tool": "cluster_ask", "args": {"question": "May I delete the old data directory?", "importance": "low"}}, {"text": "asked delete"}]},
  {"match": "ask-secret", "steps": [{"tool": "cluster_ask", "args": {"question": "What is the database password?", "importance": "low"}}, {"text": "asked secret"}]},
  {"steps": [{"text": "nothing to do"}]}
]}
JSON
mock_script qperm <<'JSON'
{"phases": [
  {"match": "was refused", "steps": [
    {"tool": "write", "args": {"filePath": "after.txt", "content": "continued without it"}},
    {"text": "finished without the command"}]},
  {"steps": [
    {"tool": "bash", "args": {"command": "rm -rf build", "description": "clean the build dir"}},
    {"text": "cleaned"}]}
]}
JSON
mock_script qbudget <<'JSON'
[
  {"tool": "write", "args": {"filePath": "one.txt", "content": "1"}},
  {"tool": "write", "args": {"filePath": "two.txt", "content": "2"}},
  {"tool": "write", "args": {"filePath": "three.txt", "content": "3"}},
  {"text": "spent more"}
]
JSON
mock_prices qtop qtool qparent qlazy qchild qperm qbudget
for a in qtop qtool qparent qlazy qchild; do agent_yaml "$W/hub" "$a" "$a"; done
agent_yaml "$W/hub" qperm qperm
agent_yaml "$W/hub" qbudget qbudget "overridable: { budget_usd: 1.0 }"
sed -i 's/^limits: { max_sessions: 4 }$/limits: { max_sessions: 4, budget_usd: 0.0002 }/' "$W/hub/agents/qbudget.yaml"
WS="$W/hub/data/workspaces"
wait_for 20 "agents loaded" bash -c "$O agents --json | python3 -c 'import sys,json;n={a[\"agent\"][\"name\"] for a in json.load(sys.stdin)};sys.exit(0 if {\"qtop\",\"qchild\",\"qbudget\"}<=n else 1)'"
# q FIELD-EXPR: the newest question matching a filter (python on dict q).
qfield() { $O questions --all --json | python3 -c "import sys,json;qs=[q for q in json.load(sys.stdin) if $1];print($2 if qs else '')"; }
export -f qfield

say "a top-level agent asks: the operator answers from the inbox"
T=$(spawn_id qtop -p "start")
wait_for 150 "the question reaches the operator (normal importance, no chain above)" bash -c "[ \"\$(qfield \"q['asker']=='$T'\" \"qs[0]['holder']\")\" = operator ]"
$O questions | sed 's/^/  /'
QT=$(qfield "q['asker']=='$T'" "qs[0]['id']")
expect "the asker shows what it is waiting on" bash -c "$O show $T | grep -q 'waiting on question(s): $QT'"
expect "a notification was raised" bash -c "$O notifications --json | grep -q '$QT'"
$O answer "$QT" --choice 2 >/dev/null
wait_for 150 "the answer arrives as a message; the agent continues" bash -c "grep -q 9090 $WS/$T/answer.txt"
expect "status answered by the operator" bash -c "[ \"\$(qfield \"q['id']=='$QT'\" \"qs[0]['status']+'/'+qs[0]['answered_by']\")\" = answered/operator:admin ]"

stop_all
say "OpenCode's own question tool"
QS=$(spawn_id qtool -p "start")
wait_for 150 "the session is blocked on its question" bash -c "[ \"\$(sess_state $QS)\" = blocked ]"
wait_for 30 "the question is in the inbox with OpenCode's options" bash -c "[ \"\$(qfield \"q['asker']=='$QS'\" \"qs[0]['kind']+':'+qs[0]['options'][1]['label']\")\" = opencode:postgres ]"
$O answer "$(qfield "q['asker']=='$QS'" "qs[0]['id']")" postgres >/dev/null
wait_for 150 "OpenCode received the reply and finished" bash -c "[ -f $WS/$QS/db.txt ] && [ \"\$(sess_state $QS)\" = idle ]"

stop_all
say "the parent answers its child's low-importance question"
P1=$(spawn_id qparent --coordinator -p "ask-low")
wait_for 150 "child asked" bash -c "[ -n \"\$(qfield \"q['asker_agent']=='qchild' and 'Tabs' in q['text']\" \"qs[0]['id']\")\" ]"
C1=$(qfield "'Tabs' in q['text']" "qs[0]['asker']")
wait_for 150 "answered by the parent, delivered to the child" bash -c "grep -q '8080 from parent' $WS/$C1/answer.txt"
expect "answered_by is the parent session" bash -c "[ \"\$(qfield \"'Tabs' in q['text']\" \"qs[0]['answered_by']\")\" = $P1 ]"

stop_all
say "escalation with a note reaches the operator"
P2=$(spawn_id qparent --coordinator -p "ask-normal")
wait_for 150 "escalated past the parent to the operator" bash -c "[ \"\$(qfield \"'port' in q['text'] and q['asker_agent']=='qchild'\" \"qs[0]['holder']\")\" = operator ]"
$O questions | sed 's/^/  /'
expect "the parent's note travels with it" bash -c "$O questions | grep -q 'parent does not know the deployment port'"
Q2=$(qfield "'port' in q['text'] and q['asker_agent']=='qchild'" "qs[0]['id']")
C2=$(qfield "q['id']=='$Q2'" "qs[0]['asker']")
$O answer "$Q2" "use 443" >/dev/null
wait_for 150 "the child gets the operator's answer" bash -c "grep -q 'use 443' $WS/$C2/answer.txt"

stop_all
say "rules raise importance; vital_only lets only vital questions through"
$O questions mode vital_only >/dev/null
P3=$(spawn_id qparent --coordinator -p "ask-delete")
wait_for 150 "a low question about deleting data is raised to vital and reaches the operator" bash -c "[ \"\$(qfield \"'delete' in q['text']\" \"qs[0]['importance']+'/'+qs[0]['holder']\")\" = vital/operator ]"
expect "the reason is recorded" bash -c "[ -n \"\$(qfield \"'delete' in q['text']\" \"' '.join(qs[0]['raised_because'])\" | grep delete)\" ]"
expect "it went through the parent first" bash -c "[ \"\$(qfield \"'delete' in q['text']\" \"qs[0]['chain'][0]['holder']\")\" = $P3 ]"
$O answer "$(qfield "'delete' in q['text']" "qs[0]['id']")" --reject >/dev/null

stop_all
say "operator-only questions skip the agents"
P4=$(spawn_id qparent --coordinator -p "ask-secret")
wait_for 150 "a question about a password goes straight to the operator" bash -c "[ \"\$(qfield \"'password' in q['text']\" \"str(qs[0]['operator_only'])+'/'+qs[0]['holder']+'/'+qs[0]['chain'][0]['why']\")\" = 'True/operator/operator only' ]"
$O answer "$(qfield "'password' in q['text']" "qs[0]['id']")" --reject >/dev/null

stop_all
say "autonomous: the top of the chain decides; the assumption log records it"
$O questions mode autonomous >/dev/null
P5=$(spawn_id qparent --coordinator -p "ask-normal")
wait_for 150 "the parent decided after nobody above would answer" bash -c "[ \"\$(qfield \"q['asker_agent']=='qchild' and q['mode']=='autonomous'\" \"qs[0]['status']+'/'+qs[0]['answered_by']\")\" = decided/$P5 ]"
C5=$(qfield "q['asker_agent']=='qchild' and q['mode']=='autonomous'" "qs[0]['asker']")
wait_for 150 "the child acts on the decision" bash -c "grep -q 'use 9090 (my decision)' $WS/$C5/answer.txt"
expect "the decision is in the assumption log" bash -c "$O assumptions --json | python3 -c 'import sys,json;sys.exit(0 if any(a[\"decided_by\"]==\"$P5\" and \"9090\" in a[\"text\"] for a in json.load(sys.stdin)) else 1)'"
$O assumptions | head -4 | sed 's/^/  /'
expect "a task can have its own mode" bash -c "TID=\$($O task add 'mode test' --label on_demand --question-mode ask_me_everything --json | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"id\"])') && $O questions mode | grep -q \"task #\$TID: ask_me_everything\""

stop_all
say "a holder that does nothing: the question moves up after answer_timeout_secs"
$O questions mode important_and_vital >/dev/null
P6=$(spawn_id qlazy --coordinator -p "ask-normal")
wait_for 150 "held by the lazy parent" bash -c "[ \"\$(qfield \"q['mode']=='important_and_vital' and q['asker_agent']=='qchild' and q['chain'][0]['holder']=='$P6'\" \"qs[0]['holder']\")\" = $P6 ]"
wait_for 40 "escalated by timeout to the operator" bash -c "[ \"\$(qfield \"q['chain'][0]['holder']=='$P6'\" \"qs[0]['holder']+'/'+qs[0]['chain'][-1]['why']\")\" = 'operator/top of the chain, mode important_and_vital, escalated by timeout' ]"
$O questions | sed 's/^/  /' 
$O answer "$(qfield "q['chain'][0]['holder']=='$P6'" "qs[0]['id']")" --decide >/dev/null
C6=$(qfield "q['chain'][0]['holder']=='$P6'" "qs[0]['asker']")
wait_for 150 "'let the agent decide' reaches the child" bash -c "grep -q 'leaves this to you' $WS/$C6/answer.txt"

stop_all
say "an ask permission becomes a question"
PP=$(spawn_id qperm -p "clean up")
wait_for 150 "rm -rf permission in the inbox as vital" bash -c "[ \"\$(qfield \"q['asker']=='$PP'\" \"qs[0]['kind']+'/'+qs[0]['importance']+'/'+qs[0]['holder']\")\" = permission/vital/operator ]"
$O answer "$(qfield "q['asker']=='$PP'" "qs[0]['id']")" reject >/dev/null
wait_for 150 "rejected; the agent is told and continues without it" bash -c "[ -f $WS/$PP/after.txt ] && [ \"\$(sess_state $PP)\" = idle ]"
expect "the rejected command did not run (its tool call failed)" bash -c "$O events --session $PP --limit 500 --json | grep '\"tool.failed\"' | grep -q bash"

stop_all
say "spending past a budget is the operator's call"
B=$(spawn_id qbudget -p "spend")
wait_for 150 "budget question raised as vital, operator only" bash -c "[ \"\$(qfield \"q['asker']=='$B'\" \"qs[0]['kind']+'/'+qs[0]['importance']+'/'+str(qs[0]['operator_only'])\")\" = budget/vital/True ]"
expect "the session is not done yet" test ! -f "$WS/$B/three.txt"
$O answer "$(qfield "q['asker']=='$B'" "qs[0]['id']")" "raise 1.00" >/dev/null
wait_for 120 "budget raised; the session finished" bash -c "[ -f $WS/$B/three.txt ]"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
