#!/usr/bin/env bash
# High-availability tests on a kind cluster with Selvedge installed (ha profile,
# test/k8s/values.yaml): a rolling restart of every runner, then the loss of a
# whole zone (the kind node of a runner that hosts the test account).
# Expects kubectl pointing at the cluster and the release `slv` in namespace `selvedge`.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
NS=selvedge
K="kubectl -n $NS"
WORK=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK"' EXIT

check() {
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; exit 1; fi
}
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
node_of() { $K get "$1" -o jsonpath='{.spec.nodeName}'; }

$K port-forward svc/slv-api 18787:8787 > /dev/null 2>&1 &
GW_PORT=18080
$K port-forward svc/slv-gateway $GW_PORT:80 > /dev/null 2>&1 & GW_PF=$!
for _ in $(seq 1 60); do curl -sf localhost:18787/healthz > /dev/null && break; sleep 1; done

ADMIN=$($K get secret slv-secrets -o jsonpath='{.data.admin-token}' | base64 -d)
API=http://localhost:18787/client/v4
admin() { curl -sf -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' "$@"; }
ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"k8s"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"k8s"}' | json '["result"]["value"]')
SUBDOMAIN=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
NAMESPACE=$(curl -sf -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -X POST \
  "$API/accounts/$ACCOUNT/storage/kv/namespaces" -d '{"title":"k8s"}' | json '["result"]["id"]')

# A KV app deployed with wrangler, and a Durable Object counter.
APP="$WORK/kv-app"
cp -r "$ROOT/test/e2e/fixtures/kv-app" "$APP"
sed "s/__NAMESPACE__/$NAMESPACE/; s/__GREETING__/k8s/" "$APP/wrangler.template.jsonc" > "$APP/wrangler.jsonc"
(cd "$APP" && CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT \
  WRANGLER_SEND_METRICS=false CI=true "$ROOT/test/e2e/node_modules/.bin/wrangler" deploy > "$WORK/deploy.log" 2>&1) \
  || { cat "$WORK/deploy.log"; exit 1; }
curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/counter" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"durable_object_namespace","name":"COUNTER","class_name":"Counter"}]};type=application/json' \
  -F "index.js=@$ROOT/test/e2e/fixtures/counter.js;filename=index.js;type=application/javascript+module" > /dev/null
sleep 4

KV_HOST="kv-app.$SUBDOMAIN.workers.local"
DO_HOST="counter.$SUBDOMAIN.workers.local"
kv() { curl -s -m 10 -o /dev/null -w '%{http_code}' -H "Host: $KV_HOST" "localhost:$GW_PORT/"; }
do_get() { curl -s -m 30 -H "Host: $DO_HOST" "localhost:$GW_PORT/?name=$1"; }
retry() {
  local out deadline=$(( $(date +%s) + 60 ))
  while :; do
    out=$(do_get "$1")
    if [[ $out =~ ^[0-9]+$ ]] || [ "$(date +%s)" -ge "$deadline" ]; then echo "$out"; return; fi
    sleep 0.5
  done
}
OBJECTS="a b c d e f g h i j k l"

check "stateless worker serves" "200" "$(kv)"
check "Durable Object counts" "1" "$(do_get first)"
for n in $OBJECTS; do do_get $n > /dev/null; done

# Stateless load in the background for the length of a disruption.
load_start() {
  rm -f "$WORK/stop" "$WORK/codes"
  ( f=0 t=0
    while [ ! -f "$WORK/stop" ]; do
      # A request that times out (in flight on a lost node) is a failure, not
      # the end of the test.
      t=$((t+1)); code=$(kv || true); [ "$code" = 200 ] || { f=$((f+1)); echo "$(date +%T) $code" >> "$WORK/codes"; }; sleep 0.05
    done
    echo "$f $t" > "$WORK/load.out" ) & LOAD=$!
}
load_stop() {
  touch "$WORK/stop"; wait $LOAD; read -r FAILS TOTAL < "$WORK/load.out"
  [ -f "$WORK/codes" ] && { echo "     failed requests:"; sed 's/^/       /' "$WORK/codes"; } || true
}

# 1. Rolling restart of every runner: graceful drains, no failed requests.
load_start
$K rollout restart deploy/slv-runner
$K rollout status deploy/slv-runner --timeout=300s
sleep 5
load_stop
check "no failed requests during a rolling restart ($TOTAL sent)" "0" "$FAILS"
for n in $OBJECTS; do check "object $n kept its state through the restart" "2" "$(retry $n)"; done

# 2. Losing a zone: kill the node of a runner that runs this account.
victim=""
for pod in $($K get pods -l app.kubernetes.io/component=runner -o name); do
  if $K logs "$pod" | grep -q "account process ready.*$ACCOUNT\|$ACCOUNT.*account process ready"; then victim=$pod; break; fi
done
[ -n "$victim" ] || { echo "FAIL no runner reports the account"; exit 1; }
victim_node=$(node_of "$victim")
echo "     killing node $victim_node (runner $victim)"
# Keep the test's own connection off the doomed node.
gw=$($K get pods -l app.kubernetes.io/component=gateway -o name | while read -r p; do [ "$(node_of "$p")" != "$victim_node" ] && echo "$p" && break; done)
kill $GW_PF; GW_PORT=18081
$K port-forward "$gw" $GW_PORT:8080 > /dev/null 2>&1 &
for _ in $(seq 1 30); do curl -s -o /dev/null "localhost:$GW_PORT/__selvedge/healthz" && break; sleep 0.5; done

load_start
docker kill "$victim_node" > /dev/null
start=$(date +%s)
for n in $OBJECTS; do check "object $n survives the zone loss" "3" "$(retry $n)"; done
echo "     every object was back within $(( $(date +%s) - start ))s (workerd's cluster lease is 20s)"
sleep 20
load_stop
echo "     stateless requests during the zone loss: $FAILS of $TOTAL failed"
# Only requests in flight on the lost node may fail.
[ "$FAILS" -le 5 ] || { echo "FAIL too many stateless failures after losing a zone"; exit 1; }
check "the first object still counts" "2" "$(retry first)"
echo "all kubernetes checks passed"
