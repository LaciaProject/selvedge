#!/usr/bin/env bash
# High-availability tests on a kind cluster with Selvedge installed (ha profile).
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

$K port-forward svc/slv-api 18787:8787 > /dev/null 2>&1 &
$K port-forward svc/slv-gateway 18080:80 > /dev/null 2>&1 &
for _ in $(seq 1 60); do curl -sf localhost:18787/healthz > /dev/null && break; sleep 1; done

ADMIN=$($K get secret slv-secrets -o jsonpath='{.data.admin-token}' | base64 -d)
API=http://localhost:18787/client/v4
admin() { curl -sf -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' "$@"; }
ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"k8s"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"k8s"}' | json '["result"]["value"]')
SUBDOMAIN=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
NAMESPACE=$(curl -sf -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -X POST \
  "$API/accounts/$ACCOUNT/storage/kv/namespaces" -d '{"title":"k8s"}' | json '["result"]["id"]')

# A stateless KV app, deployed with wrangler.
APP="$WORK/kv-app"
cp -r "$ROOT/test/e2e/fixtures/kv-app" "$APP"
sed "s/__NAMESPACE__/$NAMESPACE/; s/__GREETING__/k8s/" "$APP/wrangler.template.jsonc" > "$APP/wrangler.jsonc"
(cd "$APP" && CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT \
  WRANGLER_SEND_METRICS=false CI=true "$ROOT/test/e2e/node_modules/.bin/wrangler" deploy > "$WORK/deploy.log" 2>&1) \
  || { cat "$WORK/deploy.log"; exit 1; }

# A Durable Object counter.
curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/counter" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"durable_object_namespace","name":"COUNTER","class_name":"Counter"}]};type=application/json' \
  -F "index.js=@$ROOT/test/e2e/fixtures/counter.js;filename=index.js;type=application/javascript+module" > /dev/null
sleep 4

KV_HOST="kv-app.$SUBDOMAIN.workers.local"
DO_HOST="counter.$SUBDOMAIN.workers.local"
kv() { curl -s -m 10 -o /dev/null -w '%{http_code}' -H "Host: $KV_HOST" localhost:18080/; }
do_get() { curl -s -m 30 -H "Host: $DO_HOST" "localhost:18080/?name=$1"; }

check "stateless worker serves" "200" "$(kv)"
check "Durable Object counts" "1" "$(do_get first)"
for n in a b c d e f g h i j; do do_get $n > /dev/null; done

echo "runner pods: $($K get pods -l app.kubernetes.io/component=runner -o wide --no-headers | awk '{print $1"@"$7}' | xargs)"

# Load in the background while runners are disrupted. Stateless requests may
# not fail at all; DO requests may fail only while a crashed owner's lease runs.
load() {
  local fails=0 total=0
  while [ ! -f "$WORK/stop" ]; do
    total=$((total + 1)); [ "$(kv)" = 200 ] || fails=$((fails + 1)); sleep 0.1
  done
  echo "$fails $total" > "$WORK/load.out"
}

# 1. Rolling restart of every runner: graceful drain, zero failed requests.
load & LOAD=$!
$K rollout restart deploy/slv-runner
$K rollout status deploy/slv-runner --timeout=300s
touch "$WORK/stop"; wait $LOAD; rm "$WORK/stop"
read -r fails total < "$WORK/load.out"
check "no failed requests during a rolling restart ($total sent)" "0" "$fails"
for n in a b c d e f g h i j; do
  check "object $n kept its state through the restart" "2" "$(do_get $n)"
done

# 2. A runner pod killed without grace (a node crash, as far as workerd can tell).
victim=$($K get pods -l app.kubernetes.io/component=runner -o name | head -1)
load & LOAD=$!
$K delete "$victim" --grace-period=0 --force > /dev/null 2>&1
start=$(date +%s)
retry() { local out deadline=$(( $(date +%s) + 60 )); while :; do out=$(do_get "$1"); if [[ $out =~ ^[0-9]+$ ]] || [ "$(date +%s)" -ge $deadline ]; then echo "$out"; return; fi; sleep 0.5; done; }
for n in a b c d e f g h i j; do
  check "object $n survives a runner crash" "3" "$(retry $n)"
done
echo "     objects were back within $(( $(date +%s) - start ))s"
touch "$WORK/stop"; wait $LOAD
read -r fails total < "$WORK/load.out"
echo "     stateless requests during the crash: $fails of $total failed"
[ "$fails" -le 3 ] || { echo "FAIL too many stateless failures after a crash"; exit 1; }

check "the first object still counts" "2" "$(do_get first)"
echo "all kubernetes checks passed"
