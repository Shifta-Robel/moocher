#!/usr/bin/env bash
# Phase 4 e2e: the hub gets its HTTPS certificate by itself over ACME HTTP-01,
# against Pebble (Let's Encrypt's test CA): order, real challenge validation
# on the HTTP-01 port, then the dashboard served with the issued certificate;
# the HTTP port redirects to HTTPS and the certificate is reused after a restart.
# Needs PEBBLE_DIR with the pebble binary (v2.8) and its test certs.
source "$(dirname "$0")/lib.sh"
P=${PEBBLE_DIR:?set PEBBLE_DIR (pebble binary, localhost-cert.pem, localhost-key.pem, pebble.minica.pem)}

say "Pebble, a test ACME CA"
cat > "$S/pebble.json" <<JSON
{"pebble": {"listenAddress": "127.0.0.1:14000", "managementListenAddress": "127.0.0.1:15000",
  "certificate": "$P/localhost-cert.pem", "privateKey": "$P/localhost-key.pem",
  "httpPort": 5002, "tlsPort": 5001, "ocspResponderURL": "", "externalAccountBindingRequired": false,
  "retryAfter": {"authz": 1, "order": 1}}}
JSON
PEBBLE_VA_NOSLEEP=1 "$P/pebble" -config "$S/pebble.json" > "$S/pebble.log" 2>&1 & PIDS+=($!)
wait_for 15 "Pebble is up" curl -s --cacert "$P/pebble.minica.pem" https://localhost:14000/dir

say "the hub orders its certificate"
ENROLL_YAML="tls:
  acme:
    domains: [localhost]
    email: ops@example.org
    directory: https://localhost:14000/dir
    directory_ca: $P/pebble.minica.pem
    http01_listen: 127.0.0.1:5002" start_hub
wait_for 60 "certificate issued and cached" test -s "$W/hub/supervisor/acme/cert.pem"
curl -s --cacert "$P/pebble.minica.pem" https://localhost:15000/roots/0 > "$S/pebble-root.pem"
expect "Pebble validated the HTTP-01 challenge on the hub" grep -q 'Attempting to validate\|authz.*valid' "$S/pebble.log"
wait_for 20 "the dashboard is served with the issued certificate" curl -s --cacert "$S/pebble-root.pem" https://localhost:7700/healthz
expect "issued by Pebble for localhost" bash -c "openssl x509 -in $W/hub/supervisor/acme/cert.pem -noout -issuer -ext subjectAltName | grep -q 'Pebble' && openssl x509 -in $W/hub/supervisor/acme/cert.pem -noout -ext subjectAltName | grep -q 'DNS:localhost'"
expect "the account key and certificate key are private (0600)" bash -c "[ \"\$(stat -c %a $W/hub/supervisor/acme/key.pem)\" = 600 ] && [ \"\$(stat -c %a $W/hub/supervisor/acme/account.json)\" = 600 ]"
expect "the HTTP-01 port redirects everything else to HTTPS" bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' -H 'Host: localhost' http://127.0.0.1:5002/instances)\" = '301 https://localhost/instances' ]"
expect "audited" bash -c "OCL_URL=https://localhost:7700 OCL_CA_CERT=$S/pebble-root.pem $O audit --limit 20 | grep -q tls.certificate"

say "a restart reuses the cached certificate"
SUM=$(sha256sum < "$W/hub/supervisor/acme/cert.pem")
kill "${PIDS[2]}"; sleep 2
"$BIN/ocl-supervisor" --config-dir "$W/hub" run > "$W/supervisor2.log" 2>&1 & PIDS+=($!)
wait_for 20 "serving again" curl -s --cacert "$S/pebble-root.pem" https://localhost:7700/healthz
expect "no new order: the same certificate" bash -c "[ \"\$(sha256sum < $W/hub/supervisor/acme/cert.pem)\" = '$SUM' ] && grep -q 'loaded from cache' $W/supervisor2.log"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
