#!/usr/bin/env bash
# Upgrade test: the last release's published chart and image hold data of
# every kind, then `helm upgrade` moves the cluster to this checkout's chart
# and image (selvedge:ci) under load, the way an operator would. Every
# component rolls (API, data, gateway, runner) and the database migrates while
# the old version still runs.
#
# Checks: no stateless request fails during the upgrade (a few may, from
# releases whose gateways did not drain); the data written before is all there
# afterwards (KV, a secret, R2, Durable Object storage, D1, a queue's backlog,
# a workflow waiting for its event, static assets); restarting the new
# gateways, data and API drops nothing; and the upgraded cluster deploys.
#
# Expects UPGRADE_FROM (the release, e.g. 0.3.0) installed as `slv` in
# namespace `selvedge` with test/k8s/values.yaml and the published image, on
# the kind cluster `selvedge`, and wrangler installed in test/e2e.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
E2E="$ROOT/test/e2e"
NS=selvedge
K="kubectl -n $NS"
WORK=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK"' EXIT
: "${UPGRADE_FROM:?the release installed before the upgrade}"

check() {
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; exit 1; fi
}
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }

# NodePorts, so that requests reach the gateways and the API at a node's
# address while their pods roll (a port-forward ends with its pod).
$K expose deploy slv-api --name slv-api-np --type NodePort --port 8787 --target-port http > /dev/null
$K expose deploy slv-gateway --name slv-gateway-np --type NodePort --port 80 --target-port http > /dev/null
NODE=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' selvedge-control-plane)
API_ADDR=$NODE:$($K get svc slv-api-np -o jsonpath='{.spec.ports[0].nodePort}')
GW=$NODE:$($K get svc slv-gateway-np -o jsonpath='{.spec.ports[0].nodePort}')
API=http://$API_ADDR/client/v4
for _ in $(seq 1 60); do curl -sf "http://$API_ADDR/healthz" > /dev/null && break; sleep 1; done

version() { $K get deploy "slv-$1" -o jsonpath='{.spec.template.spec.containers[0].image}'; }
echo "     before: $(version runner)"

ADMIN=$($K get secret slv-secrets -o jsonpath='{.data.admin-token}' | base64 -d)
admin() { curl -sf -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' "$@"; }
ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"upgrade"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"upgrade"}' | json '["result"]["value"]')
auth() { curl -sf -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
SUBDOMAIN=$(auth "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
NAMESPACE=$(auth -X POST "$API/accounts/$ACCOUNT/storage/kv/namespaces" -d '{"title":"upgrade"}' | json '["result"]["id"]')
export CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT
export WRANGLER_SEND_METRICS=false CI=true
WRANGLER="$E2E/node_modules/.bin/wrangler"
get() { curl -s -m 30 -H "Host: $1.$SUBDOMAIN.workers.local" "http://$GW$2"; }
until_ok() { # host path expected: retries for up to 60 s, prints the last answer
  local out deadline=$(( $(date +%s) + 60 ))
  while :; do
    out=$(get "$1" "$2" || true)
    if [ "$out" = "$3" ] || [ "$(date +%s)" -ge "$deadline" ]; then echo "$out"; return; fi
    sleep 1
  done
}

count() { # host path: a Durable Object's answer, retried until it is a number
  local out deadline=$(( $(date +%s) + 60 ))
  while :; do
    out=$(get "$1" "$2" || true)
    if [[ $out =~ ^[0-9]+$ ]] || [ "$(date +%s)" -ge "$deadline" ]; then echo "$out"; return; fi
    sleep 1
  done
}

# ---- Data of every kind, written by the old release.
APP="$WORK/kv-app"
cp -r "$E2E/fixtures/kv-app" "$APP"
render() { sed "s/__NAMESPACE__/$NAMESPACE/; s/__GREETING__/$1/" "$APP/wrangler.template.jsonc" > "$APP/wrangler.jsonc"; }
wr() { (cd "$APP" && "$WRANGLER" "$@"); }
render before
wr deploy > "$WORK/deploy.log" 2>&1 || { cat "$WORK/deploy.log"; exit 1; }
echo -n s3cret | wr secret put TOKEN > /dev/null 2>&1
check "the old release serves a Worker with KV" "before #1" "$(until_ok kv-app / 'before #1')"
check "and its secret" "s3cret" "$(until_ok kv-app /secret s3cret)"

wr r2 bucket create upgrade-files > /dev/null 2>&1
echo -n "kept through the upgrade" > "$WORK/obj.txt"
wr r2 object put upgrade-files/seed.txt --file "$WORK/obj.txt" --remote > /dev/null 2>&1

curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/counter" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"durable_object_namespace","name":"COUNTER","class_name":"Counter"}]};type=application/json' \
  -F "index.js=@$E2E/fixtures/counter.js;filename=index.js;type=application/javascript+module" > /dev/null
OBJECTS="a b c d e f"
for n in $OBJECTS; do check "Durable Object $n counts on the old release" "1" "$(count counter "/?name=$n")"; done
check "SQLite storage on the old release" "1" "$(count counter "/rpc?name=sql")"

DB=$(auth -X POST "$API/accounts/$ACCOUNT/d1/database" -d '{"name":"upgrade-db"}' | json '["result"]["uuid"]')
d1q() { auth -X POST "$API/accounts/$ACCOUNT/d1/database/$DB/query" -d "{\"sql\":\"$1\"}"; }
d1q "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)" > /dev/null
d1q "INSERT INTO notes (body) VALUES ('one'), ('two'), ('three')" > /dev/null

wr queues create upgrade-backlog > /dev/null 2>&1
wr queues consumer http add upgrade-backlog > /dev/null 2>&1
QID=$(auth "$API/accounts/$ACCOUNT/queues?name=upgrade-backlog" | json '["result"][0]["queue_id"]')
auth -X POST "$API/accounts/$ACCOUNT/queues/$QID/messages/batch" \
  -d '{"messages":[{"body":{"n":1}},{"body":{"n":2}},{"body":{"n":3}}]}' > /dev/null

WFA="$WORK/workflow-app"
cp -r "$E2E/fixtures/workflow-app" "$WFA"
(cd "$WFA" && "$WRANGLER" deploy > "$WORK/wf.log" 2>&1) || { cat "$WORK/wf.log"; exit 1; }
WID=""
for _ in $(seq 1 30); do WID=$(get workflow-app "/start?n=21"); [ "${#WID}" = 36 ] && break; sleep 1; done
check "a workflow instance starts on the old release" "36" "${#WID}"
waiting() {
  auth "$API/accounts/$ACCOUNT/workflows/e2e-flow/instances/$WID" | python3 -c \
    'import sys,json; s=json.load(sys.stdin)["result"]["steps"]; print("waiting" if any(x["type"]=="waitForEvent" and not x["finished"] for x in s) else "")'
}
check "and waits for its event" "waiting" "$(for _ in $(seq 1 40); do [ "$(waiting)" = waiting ] && { echo waiting; break; }; sleep 0.5; done)"

ASSETS="$WORK/assets-app"
cp -r "$E2E/fixtures/assets-app" "$ASSETS"
(cd "$ASSETS" && "$WRANGLER" deploy > "$WORK/assets.log" 2>&1) || { cat "$WORK/assets.log"; exit 1; }
STYLE=$(cat "$ASSETS/public/style.css")
check "static assets on the old release" "$STYLE" "$(until_ok assets-app /style.css "$STYLE")"

# ---- The upgrade, under load: the KV app (stateless) and Durable Objects.
rm -f "$WORK/stop"
load() { # name host path: counts failures until $WORK/stop appears
  ( f=0 t=0
    while [ ! -f "$WORK/stop" ]; do
      t=$((t+1))
      code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' -H "Host: $2.$SUBDOMAIN.workers.local" "http://$GW$3" || true)
      [ "$code" = 200 ] || { f=$((f+1)); echo "$(date +%T) $code" >> "$WORK/$1.codes"; }
      sleep 0.05
    done
    echo "$f $t" > "$WORK/$1.out" ) &
}
load kv kv-app /
load do counter "/?name=load"
start=$(date +%s)
helm upgrade slv "$ROOT/deploy/helm/selvedge" --namespace "$NS" -f "$ROOT/test/k8s/values.yaml" \
  --wait --timeout 10m > "$WORK/helm.log" 2>&1 || { cat "$WORK/helm.log"; exit 1; }
for d in api data gateway runner; do $K rollout status "deploy/slv-$d" --timeout=300s > /dev/null; done
echo "     helm upgrade took $(( $(date +%s) - start ))s; now: $(version runner)"
sleep 10
touch "$WORK/stop"; wait
report() {
  read -r F T < "$WORK/$1.out"
  echo "     $2 during the upgrade: $F of $T failed"
  [ -f "$WORK/$1.codes" ] && sed 's/^/       /' "$WORK/$1.codes" | head -20 || true
}
report kv "stateless requests"
report do "Durable Object requests"
read -r KV_FAILS _ < "$WORK/kv.out"
# The gateways that stop during an upgrade run the old release. Up to 0.3.0
# they stopped without draining, dropping the requests on them and refusing
# connections kube-proxy still sent; from those, a few failures are expected.
if [ "$(printf '%s\n' "$UPGRADE_FROM" 0.3.0 | sort -V | head -1)" = "$UPGRADE_FROM" ]; then
  echo "     $UPGRADE_FROM's gateways stop without draining: up to 10 failures allowed"
  [ "$KV_FAILS" -le 10 ] || { echo "FAIL $KV_FAILS stateless requests failed during the upgrade"; exit 1; }
else
  check "no stateless request failed during the upgrade" "0" "$KV_FAILS"
fi

# ---- Everything written before is still there.
check "the image is this build's" "selvedge:ci" "$(version runner)"
COUNT=$(auth "$API/accounts/$ACCOUNT/storage/kv/namespaces/$NAMESPACE/values/count")
check "KV kept its value, and the Worker goes on from it" "before #$((COUNT + 1))" "$(get kv-app /)"
check "the secret still decrypts" "s3cret" "$(get kv-app /secret)"
check "R2 kept the object" "kept through the upgrade" "$(wr r2 object get upgrade-files/seed.txt --pipe --remote 2>/dev/null)"
for n in $OBJECTS; do check "Durable Object $n kept its state" "2" "$(count counter "/?name=$n")"; done
check "SQLite storage kept its rows" "2" "$(count counter "/rpc?name=sql")"
check "D1 kept its rows" "3" "$(d1q 'SELECT count(*) AS n FROM notes' | json '["result"][0]["results"][0]["n"]')"
check "the queue kept its backlog" '{"n":1}|{"n":2}|{"n":3}' \
  "$(auth -X POST "$API/accounts/$ACCOUNT/queues/$QID/messages/pull" -d '{"batch_size":10}' \
     | python3 -c 'import sys, json; print("|".join(sorted(m["body"] if isinstance(m["body"], str) else json.dumps(m["body"], separators=(",", ":")) for m in json.load(sys.stdin)["result"]["messages"])))')"
get workflow-app "/approve?id=$WID" > /dev/null
wf_status() {
  auth "$API/accounts/$ACCOUNT/workflows/e2e-flow/instances/$WID" | json '["result"]["status"]'
}
check "the waiting workflow completes after its event" "complete" \
  "$(for _ in $(seq 1 60); do s=$(wf_status); [ "$s" = complete ] && break; sleep 0.5; done; echo "$s")"
check "static assets are still served" "$STYLE" "$(get assets-app /style.css)"

# Restarting this build's stateless components drops nothing: gateways drain.
rm -f "$WORK/stop" "$WORK"/*.codes
load kv kv-app /
for d in gateway data api; do
  $K rollout restart "deploy/slv-$d" > /dev/null
  $K rollout status "deploy/slv-$d" --timeout=300s > /dev/null
done
sleep 5
touch "$WORK/stop"; wait
report kv "stateless requests while this build's gateways, data and API restart"
read -r KV_FAILS _ < "$WORK/kv.out"
check "no stateless request failed while they restarted" "0" "$KV_FAILS"

COUNT=$(auth "$API/accounts/$ACCOUNT/storage/kv/namespaces/$NAMESPACE/values/count")
render after
wr deploy > "$WORK/deploy2.log" 2>&1 || { cat "$WORK/deploy2.log"; exit 1; }
check "the upgraded cluster deploys" "after #$((COUNT + 1))" "$(until_ok kv-app / "after #$((COUNT + 1))")"
echo "all upgrade checks passed ($UPGRADE_FROM -> this build)"
