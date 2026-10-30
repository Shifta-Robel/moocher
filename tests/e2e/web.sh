#!/usr/bin/env bash
# Phase 4 e2e: the hub as a website. HTTPS from certificate files, password
# login with CSRF protection, lockouts and TOTP, the UI behind the login,
# "Open in OpenCode" through the hub (browser and `moo attach`), notifications
# to ntfy / a webhook / Web Push, and promoting a live route to YAML.
source "$(dirname "$0")/lib.sh"

say "a private CA and a certificate for the hub"
T=$S/tls; rm -rf "$T"; mkdir -p "$T"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj "/CN=e2e test CA" -keyout "$T/ca.key" -out "$T/ca.pem" 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=localhost -keyout "$T/key.pem" -out "$T/hub.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > "$T/ext"
openssl x509 -req -in "$T/hub.csr" -CA "$T/ca.pem" -CAkey "$T/ca.key" -CAcreateserial -days 2 -extfile "$T/ext" -out "$T/cert.pem" 2>/dev/null
[ -s "$T/cert.pem" ] && pass "certificate issued by a private CA" || fail "certificate issued"
export MOO_URL=https://127.0.0.1:7700 MOO_CA_CERT=$T/ca.pem NODE_EXTRA_CA_CERTS=$T/ca.pem
export NTFY_TOKEN=ntfy-token-$RANDOM HOOK_SECRET=hook-secret-$RANDOM
HOOKS=$S/hooks.jsonl; rm -f "$HOOKS"
python3 -I "$REPO/tests/e2e/fake_hooks.py" 8098 "$HOOKS" & PIDS+=($!)
ENROLL_YAML="tls: { cert_file: $T/cert.pem, key_file: $T/key.pem }
attach_listen: 127.0.0.1:7444
login: { max_failures: 3, lockout_secs: 60 }
notifications:
  ntfy: { url: http://127.0.0.1:8098/topic, token: \"env:NTFY_TOKEN\" }
  webhook: { url: http://127.0.0.1:8098/hook, secret: \"env:HOOK_SECRET\" }
  web_push: { subject: \"mailto:ops@example.org\" }" start_hub
C="curl -s --cacert $T/ca.pem"
H=https://127.0.0.1:7700
wait_for 20 "the API answers over HTTPS with the configured certificate" bash -c "$C $H/healthz | grep -q ok"
expect "plain HTTP is not served on the TLS port" bash -c "! curl -s -m 3 http://127.0.0.1:7700/healthz | grep -q ok"
expect "the CLI trusts the hub through MOO_CA_CERT" $O status

say "users and login"
echo "first-password-123" | "$BIN/moocher-hub" --config-dir "$W/hub" user set admin >/dev/null
echo "operator-password-456" | "$BIN/moocher-hub" --config-dir "$W/hub" user set op --scope operate >/dev/null
expect "pages need a login (redirect to /login)" bash -c "[ \"\$($C -o /dev/null -w '%{http_code} %{redirect_url}' $H/instances)\" = '302 https://127.0.0.1:7700/login?next=/instances' ]"
expect "the login page and assets are public" bash -c "[ \"\$($C -o /dev/null -w '%{http_code}' $H/login)\" = 200 ]"
expect "strict security headers on the UI" bash -c "$C -D - -o /dev/null $H/login | grep -qi \"content-security-policy: default-src 'self'\""
expect "the API refuses requests without a token or session" bash -c "[ \"\$($C -o /dev/null -w '%{http_code}' $H/api/v1/overview)\" = 401 ]"
login() { $C -c "$1" -X POST -H 'content-type: application/json' -d "$2" -o "$1.body" -w '%{http_code}' $H/auth/login; }
export C H T
export -f login
expect "a wrong password is refused" bash -c "[ \"\$(login $S/j0 '{\"user\":\"op\",\"password\":\"nope\"}')\" = 401 ]"
[ "$(login "$S/jar" '{"user":"op","password":"operator-password-456"}')" = 200 ] && pass "login works" || fail "login works"
expect "the session cookie is HttpOnly, Secure and SameSite=Strict" bash -c "$C -D - -o /dev/null -X POST -H 'content-type: application/json' -d '{\"user\":\"op\",\"password\":\"operator-password-456\"}' $H/auth/login | grep -i 'set-cookie: moo_session' | grep -i httponly | grep -i secure | grep -qi samesite=strict"
CS=$(awk '/moo_csrf/ {print $7}' "$S/jar")
expect "the session reads the API" bash -c "$C -b $S/jar $H/api/v1/info | grep -q '\"user\":\"op\"'"
expect "a change without the CSRF token is refused" bash -c "[ \"\$($C -b $S/jar -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -d '{\"title\":\"t\",\"label\":\"on_demand\"}' $H/api/v1/tasks)\" = 403 ]"
expect "with the CSRF token it works" bash -c "[ \"\$($C -b $S/jar -o /dev/null -w '%{http_code}' -X POST -H \"x-csrf-token: $CS\" -H 'content-type: application/json' -d '{\"title\":\"t\",\"label\":\"on_demand\"}' $H/api/v1/tasks)\" = 200 ]"
expect "scopes apply to browser sessions (operate cannot list users)" bash -c "[ \"\$($C -b $S/jar -o /dev/null -w '%{http_code}' $H/api/v1/users)\" = 403 ]"
for i in 1 2 3; do login "$S/j1" '{"user":"admin","password":"wrong"}' >/dev/null; done
expect "three failures lock the account, even for the right password" bash -c "[ \"\$(login $S/j1 '{\"user\":\"admin\",\"password\":\"first-password-123\"}')\" = 429 ]"
wait_for 10 "the lockout is a vital notification" bash -c "$O notifications --json | grep -q 'login locked for admin'"
expect "and audited" bash -c "$O audit --limit 50 | grep -q login.lockout"
SECRET=$("$BIN/moocher-hub" --config-dir "$W/hub" user totp op | awk '/secret:/ {print $2}')
expect "with TOTP on, the password alone asks for a code" bash -c "login $S/j2 '{\"user\":\"op\",\"password\":\"operator-password-456\"}' >/dev/null; grep -q '\"totp_required\":true' $S/j2.body"
CODE=$(python3 -c "import base64,hmac,struct,time,sys;k=base64.b32decode(sys.argv[1]+'='*(-len(sys.argv[1])%8));h=hmac.new(k,struct.pack('>Q',int(time.time())//30),'sha1').digest();o=h[19]&15;print('%06d'%((struct.unpack('>I',h[o:o+4])[0]&0x7fffffff)%1000000))" "$SECRET")
expect "password + code logs in" bash -c "[ \"\$(login $S/j3 '{\"user\":\"op\",\"password\":\"operator-password-456\",\"totp\":\"$CODE\"}')\" = 200 ]"
expect "the same code does not work twice" bash -c "[ \"\$(login $S/j4 '{\"user\":\"op\",\"password\":\"operator-password-456\",\"totp\":\"$CODE\"}')\" = 401 ]"
expect "logout ends the session" bash -c "$C -b $S/jar -X POST -H \"x-csrf-token: $CS\" $H/auth/logout >/dev/null && [ \"\$($C -b $S/jar -o /dev/null -w '%{http_code}' $H/api/v1/info)\" = 401 ]"

say "Open in OpenCode through the hub (TLS on the attach port too)"
SID=$(spawn_id builder -p "Write greet(name) in greet.py with pytest tests.")
wait_for 150 "builder finished its turn" bash -c "[ \"\$(sess_state $SID)\" = idle ]"
URL=$($C -X POST -H "authorization: Bearer $MOO_TOKEN" $H/api/v1/sessions/$SID/attach | python3 -c 'import sys,json;print(json.load(sys.stdin)["url"])')
expect "the ticket URL is on the attach origin over https" bash -c "echo '$URL' | grep -q '^https://127.0.0.1:7444/__moo/attach?ticket='"
node "$REPO/tests/e2e/attach-check.mjs" "$URL" > "$S/attach.out" 2>&1; sed 's/^/  /' "$S/attach.out"
expect "every attach check passed" bash -c "! grep -q FAIL $S/attach.out && grep -q 'terminal over WebSocket works' $S/attach.out"
expect "the instance shows who attached" bash -c "[ \"\$(sess 's.get(\"attached_by\")' $SID)\" = operator:admin ]"
$O attach "$SID" --no-launch > "$S/mooattach.out" 2>&1 & AP=$!; PIDS+=($AP)
wait_for 20 "moo attach opened a local proxy" bash -c "grep -q 'local address http://127.0.0.1:' $S/mooattach.out"
LOCAL=$(grep -o 'http://127.0.0.1:[0-9]*' "$S/mooattach.out" | head -1)
expect "through moo attach, OpenCode answers without any password on this side" bash -c "curl -s $LOCAL/api/health | grep -q '\"healthy\":true'"
expect "repeated requests on one connection keep working (cookie added each time)" bash -c "curl -s $LOCAL/api/health $LOCAL/session | grep -c healthy | grep -q 1"
kill $AP 2>/dev/null

say "notifications: ntfy, webhook and Web Push"
node "$REPO/tests/e2e/push-check.mjs" "$H" "$MOO_TOKEN" 8097 > "$S/push.out" 2>&1; sed 's/^/  /' "$S/push.out"
expect "every Web Push check passed" bash -c "! grep -q FAIL $S/push.out && grep -q 'decrypted payload' $S/push.out"
wait_for 10 "ntfy received it with title, priority and token" bash -c "python3 -c 'import json,sys;l=[json.loads(x) for x in open(\"$HOOKS\")];sys.exit(0 if any(r[\"path\"]==\"/topic\" and r[\"headers\"].get(\"title\")==\"Test notification\" and r[\"headers\"].get(\"priority\")==\"4\" and r[\"headers\"].get(\"authorization\")==\"Bearer $NTFY_TOKEN\" for r in l) else 1)'"
expect "the webhook got JSON with a valid HMAC signature" bash -c "python3 -c 'import json,sys,hmac,hashlib;l=[json.loads(x) for x in open(\"$HOOKS\")];r=[r for r in l if r[\"path\"]==\"/hook\" and \"Test notification\" in r[\"body\"]][0];sig=hmac.new(b\"$HOOK_SECRET\",r[\"body\"].encode(),hashlib.sha256).hexdigest();sys.exit(0 if r[\"headers\"][\"x-moo-signature\"]==\"sha256=\"+sig and json.loads(r[\"body\"])[\"title\"]==\"Test notification\" else 1)'"
expect "vital events (the lockout) went out too" bash -c "grep -q 'login locked for admin' $HOOKS"
expect "info-level events stay in-app by default" bash -c "! grep -q 'task.done\\|node.back' $HOOKS"

say "a live route change, then written to YAML"
$O route set hub builder --prefer direct >/dev/null
wait_for 10 "override in overrides.yaml" grep -q builder "$W/hub/overrides.yaml"
expect "promote writes route: into agents/builder.yaml" bash -c "$C -X POST -H \"authorization: Bearer $MOO_TOKEN\" -H 'content-type: application/json' -d '{\"agent\":\"builder\"}' $H/api/v1/nodes/hub/routes/promote | grep -q '\"ok\":true' && grep -q '^route: {\"prefer\":\"direct\"' $W/hub/agents/builder.yaml"
expect "and drops the override" bash -c "! grep -q 'builder' $W/hub/overrides.yaml"
expect "the file still loads (agent ready)" bash -c "$O agents --json | python3 -c 'import sys,json;sys.exit(0 if any(a[\"agent\"][\"name\"]==\"builder\" and not a[\"agent\"][\"errors\"] for a in json.load(sys.stdin)) else 1)'"

[ "$FAILED" = 0 ] && say "ALL PASSED" || { say "SOME CHECKS FAILED"; exit 1; }
