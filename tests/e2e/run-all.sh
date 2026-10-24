#!/usr/bin/env bash
# Run every e2e suite; prints a summary and exits non-zero if any failed.
cd "$(dirname "$0")"
rc=0
# Optional suites need extra tools: acme (PEBBLE_DIR), ui (PW_DIR + Chromium).
suites="enroll relay quota templates stats resilience artifacts questions death harvest web metering egress"
[ -n "${PEBBLE_DIR:-}" ] && suites="$suites acme"
[ -n "${PW_DIR:-}" ] && suites="$suites ui"
for t in $suites; do
  if ./$t.sh > "${OCL_E2E_DIR:-/tmp/ocl-e2e}/$t.out" 2>&1; then
    printf '%-12s ok    (%s checks)\n' "$t" "$(grep -c "32mPASS" "${OCL_E2E_DIR:-/tmp/ocl-e2e}/$t.out")"
  else
    printf '%-12s FAIL  see %s\n' "$t" "${OCL_E2E_DIR:-/tmp/ocl-e2e}/$t.out"
    rc=1
  fi
done
exit $rc
