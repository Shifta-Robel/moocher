#!/usr/bin/env bash
# Phase 5 e2e: operations. Daily event export (incremental, zstd frames),
# pruning only what was exported, artifact expiry that spares tagged and
# referenced artifacts, and `disk_enforce: hard` refusing to start where
# quotas cannot work.
ENROLL_YAML=$'artifacts: { retain: 1d }\nevents: { retain: 30d }'
source "$(dirname "$0")/lib.sh"
SUP="$BIN/ocl-supervisor"
EXP="$W/hub/supervisor/exports"
lines() { cat "$EXP"/events-*.jsonl.zst 2>/dev/null > "$W/all.zst"; "$SUP" --config-dir "$W/hub" events-cat "$W/all.zst" | wc -l; }
hk() { $O housekeeping --json "$@"; }
field() { python3 -c "import sys,json;v=json.load(sys.stdin);print(v['$1'] if not isinstance(v['$1'],list) else len(v['$1']))"; }

say "hub"
start_hub
wait_for 20 "hub node connected" is_connected hub

say "events are exported by day, incrementally"
TODAY=$(date -u +%Y%m%d)
hk | field events_exported > "$W/n0"
[ "$(cat "$W/n0")" = 0 ] && pass "today's events wait for the day to end" || fail "nothing exported before the day ends (got $(cat "$W/n0"))"
N1=$(hk --ahead 1d | field events_exported)
[ "$N1" -gt 0 ] && [ -f "$EXP/events-$TODAY.jsonl.zst" ] && pass "run as of tomorrow: $N1 events in events-$TODAY.jsonl.zst" || fail "export as of tomorrow (got $N1)"
[ "$(lines)" = "$N1" ] && pass "the file decodes to $N1 JSON lines" || fail "decoded line count ($(lines) vs $N1)"
"$SUP" --config-dir "$W/hub" events-cat "$EXP/events-$TODAY.jsonl.zst" | head -1 | python3 -c 'import sys,json;e=json.loads(sys.stdin.readline());sys.exit(0 if {"id","ts","kind"}<=set(e) else 1)' \
  && pass "each line is an event with id, ts and kind" || fail "event line shape"
[ "$(hk --ahead 1d | field events_exported)" = 0 ] && pass "a second run exports nothing twice" || fail "second run is idempotent"
$O artifact put "$W/hub/supervisor.yaml" --name later >/dev/null # more events today
N2=$(hk --ahead 1d | field events_exported)
[ "$N2" -gt 0 ] && [ "$(lines)" = $((N1 + N2)) ] && pass "later events are appended as a new frame ($N1 + $N2 lines)" || fail "append (got $N2, lines $(lines))"

say "pruning removes only exported events past retain"
BEFORE=$($O events --limit 100000 --json | wc -l)
DRY=$(hk --ahead 31d --dry-run)
[ "$(echo "$DRY" | field events_pruned)" = 0 ] && [ "$($O events --limit 100000 --json | wc -l)" -ge "$BEFORE" ] && pass "a dry run changes nothing" || fail "dry run changes nothing"
P=$(hk --ahead 31d | field events_pruned)
AFTER=$($O events --limit 100000 --json | wc -l)
[ "$P" -gt 0 ] && [ "$AFTER" -lt "$BEFORE" ] && pass "$P exported events pruned from the database ($BEFORE -> $AFTER)" || fail "events pruned (pruned $P, $BEFORE -> $AFTER)"
[ "$(lines)" -ge $((N1 + N2)) ] && pass "the export still holds them" || fail "export kept"

say "artifact expiry (retain 1d): tagged and referenced ones stay"
echo plain > "$W/a.txt"; echo kept > "$W/b.txt"; echo seed > "$W/c.txt"
ref() { python3 -c 'import sys,json;print(json.load(sys.stdin)["reference"])'; }
A=$($O artifact put "$W/a.txt" --json | ref)
B=$($O artifact put "$W/b.txt" --tag keep --json | ref)
C=$($O artifact put "$W/c.txt" --json | ref)
agent_yaml "$W/hub" seeded builder "workspace: { seed: \"$C\" }"
R=$(hk --ahead 2d --dry-run)
echo "$R" | python3 -c "import sys,json;r=json.load(sys.stdin);sys.exit(0 if '$A' in r['artifacts_removed'] and '$B' not in r['artifacts_removed'] and '$C' not in r['artifacts_removed'] else 1)" \
  && pass "dry run: would remove the plain one only" || fail "dry run selection: $R"
$O artifact show "$A" >/dev/null 2>&1 && pass "and removed nothing" || fail "dry run removed nothing"
hk --ahead 2d > "$W/gc.json"
$O artifact show "$A" >/dev/null 2>&1 && fail "plain artifact expired" || pass "plain artifact expired"
H=${A#art://b3/}
[ ! -e "$W/hub/supervisor/artifacts/b3/${H:0:2}/$H" ] && pass "its blob is gone" || fail "its blob is gone"
$O artifact show "$B" >/dev/null && pass "the one tagged keep stays" || fail "tagged keep stays"
$O artifact show "$C" >/dev/null && pass "the one an agent profile seeds from stays" || fail "referenced stays"
$O audit --limit 20 2>/dev/null | grep -q housekeeping && pass "housekeeping is audited" || fail "housekeeping is audited"
$O events --limit 100000 | grep -q artifact.expired && pass "artifact.expired event" || fail "artifact.expired event"

say "disk_enforce: hard refuses to start where quotas cannot work"
mkdir -p "$W/hard"
cat > "$W/hard/node.yaml" <<EOF
name: hard
supervisor: unix://$W/run/nowhere.sock
disk_enforce: hard
runtime: { kind: process, opencode_bin: $OC, run_dir: $(run_dir hard) }
EOF
set +e
timeout 20 "$BIN/opencode-node" --config-dir "$W/hard" run > "$W/hard.log" 2>&1
RC=$?
set -e
[ "$RC" != 0 ] && [ "$RC" != 124 ] && grep -q "disk_enforce: hard needs" "$W/hard.log" && pass "refused: $(grep -o 'disk_enforce: hard needs[^"]*' "$W/hard.log" | head -1)" || fail "hard mode refused (rc $RC): $(tail -2 "$W/hard.log")"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
