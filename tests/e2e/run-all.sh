#!/usr/bin/env bash
# Run every e2e suite; prints a summary and exits non-zero if any failed.
cd "$(dirname "$0")"
rc=0
for t in enroll relay quota templates stats resilience; do
  if ./$t.sh > "${OCL_E2E_DIR:-/tmp/ocl-e2e}/$t.out" 2>&1; then
    printf '%-12s ok    (%s checks)\n' "$t" "$(grep -c PASS "${OCL_E2E_DIR:-/tmp/ocl-e2e}/$t.out")"
  else
    printf '%-12s FAIL  see %s\n' "$t" "${OCL_E2E_DIR:-/tmp/ocl-e2e}/$t.out"
    rc=1
  fi
done
exit $rc
