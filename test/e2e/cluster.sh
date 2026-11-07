#!/usr/bin/env bash
# Durable Objects on two runners in workerd cluster mode, and failover when one
# runner dies. Needs SELVEDGE_DATABASE_URL, SELVEDGE_WORKERD and an S3 bucket
# with conditional writes at DO_S3_ENDPOINT (bucket DO_S3_BUCKET).
# SELVEDGE_E2E_TLS=1 runs every internal hop with mTLS.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
WORK=$(mktemp -d)
BIN=${SELVEDGE_BIN:-$ROOT/target/debug/selvedge}
P=${PORT_BASE:-28600}
RUN=${XDG_RUNTIME_DIR:-/tmp}/slv-cl-$$

export SELVEDGE_BLOB_URL="file://$WORK/blobs"
export SELVEDGE_MASTER_KEY=$(openssl rand -hex 32)
export SELVEDGE_INTERNAL_TOKEN=$(openssl rand -hex 16)
export SELVEDGE_ADMIN_TOKEN=admin-$(openssl rand -hex 8)
export SELVEDGE_WORKERS_DEV_ZONE=workers.test
PUBLIC_API=http://127.0.0.1:$((P + 87))
export SELVEDGE_API_LISTEN=127.0.0.1:$((P + 87)) SELVEDGE_API_URL=$PUBLIC_API
export SELVEDGE_DATA_LISTEN=127.0.0.1:$((P + 88)) SELVEDGE_DATA_ADDR=127.0.0.1:$((P + 88))
export SELVEDGE_GATEWAY_URL=http://127.0.0.1:$((P + 80))
if [ "${SELVEDGE_E2E_TLS:-}" = 1 ]; then
  # A platform CA and one certificate per role, <role>.selvedge.internal.
  T="$WORK/tls"; mkdir -p "$T"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$T/ca.key" -out "$T/ca.pem" \
    -days 2 -subj /CN=selvedge-e2e-ca 2>/dev/null
  for role in api data gateway runner; do
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$T/$role.key" -out "$T/$role.csr" \
      -subj "/CN=$role" 2>/dev/null
    printf 'subjectAltName=DNS:%s.selvedge.internal\nextendedKeyUsage=serverAuth,clientAuth\n' "$role" > "$T/ext"
    openssl x509 -req -in "$T/$role.csr" -CA "$T/ca.pem" -CAkey "$T/ca.key" -CAcreateserial -out "$T/$role.pem" \
      -days 2 -extfile "$T/ext" 2>/dev/null
  done
  # A certificate from another CA, which the platform must refuse.
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$T/rogue.key" -out "$T/rogue.pem" \
    -days 2 -subj /CN=rogue -addext subjectAltName=DNS:gateway.selvedge.internal 2>/dev/null
  export SELVEDGE_TLS_CA="$T/ca.pem"
  role() { echo "SELVEDGE_TLS_CERT=$T/$1.pem SELVEDGE_TLS_KEY=$T/$1.key"; }
  export SELVEDGE_API_INTERNAL_LISTEN=127.0.0.1:$((P + 86)) SELVEDGE_API_URL=https://127.0.0.1:$((P + 86))
  export SELVEDGE_GATEWAY_INTERNAL_LISTEN=127.0.0.1:$((P + 79)) SELVEDGE_GATEWAY_URL=https://127.0.0.1:$((P + 79))
else
  role() { :; }
fi
BUCKET=${DO_S3_BUCKET:-selvedge-do}
export SELVEDGE_DO_BUCKET="s3://$BUCKET/cluster-$$"
export SELVEDGE_DO_BUCKET_OPTIONS="aws_endpoint=${DO_S3_ENDPOINT:-http://127.0.0.1:29000},aws_region=us-east-1,aws_access_key_id=${DO_S3_KEY:-any},aws_secret_access_key=${DO_S3_SECRET:-any}"
export SELVEDGE_CLUSTER_SECRET=$(openssl rand -hex 32)
export SELVEDGE_ACCOUNT_MEMORY_MB=512

PIDS=()
start() { # name, args...
  local name=$1; shift
  "$@" > "$WORK/$name.log" 2>&1 &
  PIDS+=($!)
  eval "PID_$name=$!"
}
agent() { # name, port
  SELVEDGE_AGENT_LISTEN=127.0.0.1:$2 SELVEDGE_STATE_DIR="$WORK/$1" SELVEDGE_RUN_DIR="$RUN/$1" \
    start "$1" env $(role runner) "$BIN" agent
}
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  pkill -f "$WORK/" 2>/dev/null || true
  wait 2>/dev/null || true
  if [ "${FAILED:-0}" = 1 ]; then for f in "$WORK"/*.log; do echo "== $f"; tail -30 "$f"; done; fi
  rm -rf "$WORK" "$RUN"
}
trap 'FAILED=1' ERR
trap cleanup EXIT

start api env $(role api) "$BIN" api
start data env $(role data) "$BIN" data
for _ in $(seq 1 50); do curl -sf "$PUBLIC_API/healthz" >/dev/null && break; sleep 0.2; done
agent a1 $((P + 91))
agent a2 $((P + 92))
SELVEDGE_GATEWAY_LISTEN=127.0.0.1:$((P + 80)) SELVEDGE_RUNNERS=127.0.0.1:$((P + 91)),127.0.0.1:$((P + 92)) \
  start gw env $(role gateway) "$BIN" gateway
sleep 3

API=$PUBLIC_API/client/v4
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
admin() { curl -sf -H "Authorization: Bearer $SELVEDGE_ADMIN_TOKEN" -H 'content-type: application/json' "$@"; }
check() {
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; FAILED=1; exit 1; fi
}
if [ "${SELVEDGE_E2E_TLS:-}" = 1 ]; then
  AGENT=127.0.0.1:$((P + 91))
  code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$@" || true; }
  check "an agent refuses plain HTTP" "000" "$(code "http://$AGENT/healthz")"
  check "an agent refuses a client without a certificate" "000" "$(code -k "https://$AGENT/healthz")"
  check "an agent refuses a certificate from another CA" "000" \
    "$(code -k --cert "$T/rogue.pem" --key "$T/rogue.key" "https://$AGENT/healthz")"
  as() { # client role, server role, port, path: HTTPS with the client role's certificate
    code --cacert "$T/ca.pem" --cert "$T/$1.pem" --key "$T/$1.key" -H "Authorization: Bearer $SELVEDGE_INTERNAL_TOKEN" \
      --resolve "$2.selvedge.internal:$3:127.0.0.1" "https://$2.selvedge.internal:$3$4"
  }
  check "an agent accepts the gateway's certificate" "200" "$(as gateway runner $((P + 91)) /healthz)"
  check "an agent refuses the data service's certificate" "000" "$(as data runner $((P + 91)) /healthz)"
  check "an agent lets its own role reach /healthz (probes)" "200" "$(as runner runner $((P + 91)) /healthz)"
  check "an agent lets its own role reach nothing else" "403" "$(as runner runner $((P + 92)) /anything)"
  check "a runner cannot reach another runner's accounts through /healthz" "ok" \
    "$(curl -s --max-time 5 --cacert "$T/ca.pem" --cert "$T/runner.pem" --key "$T/runner.key" \
      -H "x-selvedge-account: 0123456789abcdef0123456789abcdef" -H "x-selvedge-script: counter" \
      -H "x-selvedge-token: $SELVEDGE_INTERNAL_TOKEN" --resolve "runner.selvedge.internal:$((P + 92)):127.0.0.1" \
      "https://runner.selvedge.internal:$((P + 92))/healthz")"
  check "the gateway's internal port refuses a runner's certificate" "000" "$(as runner gateway $((P + 79)) /__selvedge/healthz)"
  check "the gateway's internal port accepts the data service's certificate" "200" "$(as data gateway $((P + 79)) /__selvedge/healthz)"
  check "the data service refuses the gateway's certificate" "000" "$(as gateway data $((P + 88)) /healthz)"
  check "the data service accepts a runner's certificate" "200" "$(as runner data $((P + 88)) /healthz)"
  check "the API's internal port refuses the data service's certificate" "000" "$(as data api $((P + 86)) /healthz)"
  check "the API's internal port accepts a runner's certificate" "200" "$(as runner api $((P + 86)) /healthz)"
  check "the public API port does not serve internal endpoints" "404" \
    "$(code -H "Authorization: Bearer $SELVEDGE_INTERNAL_TOKEN" "$PUBLIC_API/internal/v1/routes")"
fi
ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"cluster"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"t"}' | json '["result"]["value"]')
SUBDOMAIN=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/counter" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"durable_object_namespace","name":"COUNTER","class_name":"Counter"}]};type=application/json' \
  -F "index.js=@$ROOT/test/e2e/fixtures/counter.js;filename=index.js;type=application/javascript+module" > /dev/null

curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/room" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"durable_object_namespace","name":"ROOM","class_name":"Room"}]};type=application/json' \
  -F "index.js=@$ROOT/test/e2e/fixtures/room.js;filename=index.js;type=application/javascript+module" > /dev/null
sleep 3
ROOM="room.$SUBDOMAIN.workers.test"
check "WebSocket to a hibernatable Durable Object" "echo:hi" "$(python3 "$ROOT/test/e2e/ws.py" $((P + 80)) "$ROOM" /ws hi)"
curl -s -H "Host: $ROOM" "http://127.0.0.1:$((P + 80))/alarm" > /dev/null
sleep 3
check "a Durable Object alarm fires" "1" "$(curl -s -H "Host: $ROOM" "http://127.0.0.1:$((P + 80))/")"
HOST="counter.$SUBDOMAIN.workers.test"
get() { curl -s -m 30 -H "Host: $HOST" "http://127.0.0.1:$((P + 80))$1"; }

# The gateway spreads requests over both runners; the count must stay one sequence.
seq_out=""
for i in $(seq 1 10); do seq_out="$seq_out $(get /)"; done
check "one owner across runners" " 1 2 3 4 5 6 7 8 9 10" "$seq_out"
check "JS RPC with SQLite storage" "1" "$(get /rpc)"

# A process over its memory limit drains when it is restarted, so its Durable
# Objects move at once instead of after the cluster lease (20 seconds).
direct() { # agent port, path: a request straight to one runner, as the gateway sends it
  local h=(-H "x-selvedge-token: $SELVEDGE_INTERNAL_TOKEN" -H "x-selvedge-account: $ACCOUNT" -H 'x-selvedge-script: counter')
  if [ "${SELVEDGE_E2E_TLS:-}" = 1 ]; then
    curl -s -m 10 "${h[@]}" --cacert "$T/ca.pem" --cert "$T/gateway.pem" --key "$T/gateway.key" \
      --resolve "runner.selvedge.internal:$1:127.0.0.1" "https://runner.selvedge.internal:$1$2"
  else
    curl -s -m 10 "${h[@]}" "http://127.0.0.1:$1$2"
  fi
}
for port in $((P + 91)) $((P + 92)); do
  last=0 restarted=no
  for _ in $(seq 1 40); do
    n=$(direct "$port" /grow)
    if [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -le "$last" ]; then restarted=yes; break; fi
    [[ "$n" =~ ^[0-9]+$ ]] && last=$n
    sleep 0.3
  done
  check "runner $port restarts its account process over the memory limit" "yes" "$restarted"
done
t0=$(date +%s)
check "Durable Objects answer at once after limit restarts, with their state" "11" "$(get /)"
check "...within 8 seconds" "yes" "$([ $(( $(date +%s) - t0 )) -le 8 ] && echo yes || echo no)"
if [ "${SELVEDGE_E2E_TLS:-}" = 1 ]; then
  check "the public gateway port ignores internal routing, even with the token" "404" \
    "$(curl -s -o /dev/null -w '%{http_code}' -H "x-selvedge-token: $SELVEDGE_INTERNAL_TOKEN" \
      -H "x-selvedge-account: $ACCOUNT" -H 'x-selvedge-script: counter' -H 'Host: nowhere.test' "http://127.0.0.1:$((P + 80))/")"
fi

# Durable Object storage goes through the agent's S3 proxy: workerd holds a
# key for its own account only, which reaches only that account's prefix.
PROXY=$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/a2.log" | grep -o 'storage proxy listening addr=[0-9.:]*' | head -1 | cut -d= -f2)
WPID=$(pgrep -f "^.*workerd serve.*$WORK/a2/" | head -1)
check "workerd holds only its account's storage key" "$ACCOUNT no-real-secret" \
  "$(tr '\0' '\n' < /proc/"$WPID"/environ | sed -n 's/^AWS_ACCESS_KEY_ID=//p') $(tr '\0' '\n' < /proc/"$WPID"/environ | grep -q "^AWS_SECRET_ACCESS_KEY=${DO_S3_SECRET:-any}$" && echo real-secret || echo no-real-secret)"
OWN_KEY=$(tr '\0' '\n' < /proc/"$WPID"/environ | sed -n 's/^AWS_SECRET_ACCESS_KEY=//p')
OTHER=$(openssl rand -hex 16)
PFX="cluster-$$"
s3() { python3 "$ROOT/test/e2e/s3req.py" "http://$PROXY" "$ACCOUNT" "$1" "$2" "$3" "${4:-}"; }
check "the proxy lists the account's own prefix" "200" "$(s3 "$OWN_KEY" GET "/$BUCKET/" "list-type=2&prefix=$PFX/$ACCOUNT/")"
check "the proxy refuses another account's prefix" "403" "$(s3 "$OWN_KEY" GET "/$BUCKET/" "list-type=2&prefix=$PFX/$OTHER/")"
check "the proxy refuses another account's objects" "403" "$(s3 "$OWN_KEY" GET "/$BUCKET/$PFX/$OTHER/v1/x")"
check "the proxy refuses a bucket-wide listing" "403" "$(s3 "$OWN_KEY" GET "/$BUCKET/" "list-type=2")"
check "the proxy refuses a bad signature" "403" "$(s3 "not-the-key" GET "/$BUCKET/" "list-type=2&prefix=$PFX/$ACCOUNT/")"

# A D1 database lives in a Durable Object too.
DB=$(curl -sf -X POST "$API/accounts/$ACCOUNT/d1/database" -H "Authorization: Bearer $TOKEN" -d '{"name":"crash"}' | json '["result"]["uuid"]')
d1() { curl -sf -X POST "$API/accounts/$ACCOUNT/d1/database/$DB/query" -H "Authorization: Bearer $TOKEN" -d "$1"; }
d1 '{"sql":"CREATE TABLE t (v TEXT)"}' > /dev/null
d1 '{"sql":"INSERT INTO t VALUES (?)","params":["kept"]}' > /dev/null

# Workflow instances run on Durable Objects too: start some that wait for an
# event, so some are owned by the runner about to die.
curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/workflow-app" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"workflow","name":"FLOW","workflow_name":"crash-flow","class_name":"Flow"}]};type=application/json' \
  -F "index.js=@$ROOT/test/e2e/fixtures/workflow-app/index.js;filename=index.js;type=application/javascript+module" > /dev/null
curl -sf -X PUT "$API/accounts/$ACCOUNT/workflows/crash-flow" -H "Authorization: Bearer $TOKEN" \
  -d '{"script_name":"workflow-app","class_name":"Flow"}' > /dev/null
wf() { curl -s -H "Host: workflow-app.$SUBDOMAIN.workers.test" "http://127.0.0.1:$((P + 80))$1"; }
WIDS=()
for n in 1 2 3 4 5 6; do
  id=""
  for _ in $(seq 1 20); do id=$(wf "/start?n=$n"); [ ${#id} = 36 ] && break; sleep 0.5; done
  WIDS+=("$id")
done
check "workflow instances start on two runners" "6" "$(printf '%s\n' "${WIDS[@]}" | grep -c '^.\{36\}$')"

# Spread objects over both runners, then kill one runner and its workerd.
for n in a b c d e f g h; do get "/?name=$n" > /dev/null; done
kill -9 "$PID_a1"; pkill -9 -f "$WORK/a1/" || true
start_ts=$(date +%s)
# Requests for objects the dead runner owned fail (like Cloudflare's "Network
# connection lost") until its lease expires and the survivor takes them over.
# A client retries; no write may be lost or applied twice.
retry_get() {
  local out deadline=$(( $(date +%s) + 45 ))
  while :; do
    out=$(get "$1")
    if [[ $out =~ ^[0-9]+$ ]] || [ "$(date +%s)" -ge "$deadline" ]; then echo "$out"; return; fi
    sleep 0.5
  done
}
for n in a b c d e f g h; do
  check "object $n survives a runner crash" "2" "$(retry_get "/?name=$n")"
done
echo "     every object was back within $(( $(date +%s) - start_ts ))s (workerd's cluster lease is 20s)"
check "the first object kept its count" "12" "$(retry_get /)"
d1_value() {
  local deadline=$(( $(date +%s) + 45 )) out
  while :; do
    out=$(d1 '{"sql":"SELECT v FROM t"}' 2>/dev/null | json '["result"][0]["results"][0]["v"]' 2>/dev/null) && { echo "$out"; return; }
    [ "$(date +%s)" -ge "$deadline" ] && { echo "unavailable"; return; }
    sleep 0.5
  done
}
check "D1 data survives a runner crash" "kept" "$(d1_value)"

# The waiting instances carry on after the crash: an event completes each.
wf_done=0
for id in "${WIDS[@]}"; do
  deadline=$(( $(date +%s) + 45 ))
  until [ "$(wf "/approve?id=$id")" = approved ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
done
for id in "${WIDS[@]}"; do
  for _ in $(seq 1 60); do
    st=$(wf "/status?id=$id" | python3 -c 'import sys,json; print(json.load(sys.stdin)["status"])' 2>/dev/null)
    [ "$st" = complete ] && { wf_done=$((wf_done + 1)); break; }
    sleep 0.5
  done
done
check "workflow instances survive a runner crash" "6" "$wf_done"

# Static assets are cached on the runner, so a site keeps serving while the
# data service is down.
SITE="$WORK/site"
cp -r "$ROOT/test/e2e/fixtures/assets-app" "$SITE"
(cd "$SITE" && CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT \
  WRANGLER_SEND_METRICS=false CI=true "$ROOT/test/e2e/node_modules/.bin/wrangler" deploy -c site.jsonc) \
  > "$WORK/site-deploy.log" 2>&1 || { cat "$WORK/site-deploy.log"; exit 1; }
SUB=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
site() { curl -s -H "Host: static-site.$SUB.workers.test" "http://127.0.0.1:$((P + 80))$1"; }
site_out=""
for _ in $(seq 1 20); do site_out=$(site /); [ "$site_out" = "<h1>spa</h1>" ] && break; sleep 0.5; done
check "a static site is served" "<h1>spa</h1>" "$site_out"
kill "$PID_data"; wait "$PID_data" 2>/dev/null || true
check "cached assets are served while the data service is down" "<h1>spa</h1>" "$(site /deep/link)"
echo "all cluster checks passed"
