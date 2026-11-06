#!/usr/bin/env bash
# End-to-end test of the minimal profile: `selvedge all` on one node, driven by
# the official wrangler CLI. Needs SELVEDGE_DATABASE_URL and SELVEDGE_WORKERD;
# run `npm install` in test/e2e first.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
E2E="$ROOT/test/e2e"
WORK=$(mktemp -d)
BIN=${SELVEDGE_BIN:-$ROOT/target/debug/selvedge}
PORT_BASE=${PORT_BASE:-28700}
API_PORT=$((PORT_BASE + 87)) GATEWAY_PORT=$((PORT_BASE + 80))

export SELVEDGE_BLOB_URL="file://$WORK/blobs"
export SELVEDGE_MASTER_KEY=$(openssl rand -hex 32)
export SELVEDGE_INTERNAL_TOKEN=$(openssl rand -hex 16)
export SELVEDGE_ADMIN_TOKEN=admin-$(openssl rand -hex 8)
export SELVEDGE_WORKERS_DEV_ZONE=workers.test
export SELVEDGE_STATE_DIR="$WORK/state"
export SELVEDGE_RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/slv-e2e-$$"
export SELVEDGE_API_LISTEN=127.0.0.1:$API_PORT
export SELVEDGE_DATA_LISTEN=127.0.0.1:$((PORT_BASE + 88))
export SELVEDGE_AGENT_LISTEN=127.0.0.1:$((PORT_BASE + 89))
export SELVEDGE_GATEWAY_LISTEN=127.0.0.1:$GATEWAY_PORT

"$BIN" all > "$WORK/selvedge.log" 2>&1 &
PID=$!
cleanup() {
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  if [ "${KEEP_LOGS:-}" = 1 ] || [ "${FAILED:-0}" = 1 ]; then cat "$WORK/selvedge.log" | tail -50; fi
  rm -rf "$WORK" "$SELVEDGE_RUN_DIR"
}
trap 'FAILED=1' ERR
trap cleanup EXIT

API=http://127.0.0.1:$API_PORT/client/v4
for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:$API_PORT/healthz" >/dev/null && break; sleep 0.2; done

json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
admin() { curl -sf -H "Authorization: Bearer $SELVEDGE_ADMIN_TOKEN" -H 'content-type: application/json' "$@"; }
check() { # description, expected, actual
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; FAILED=1; exit 1; fi
}

ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"e2e"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"e2e"}' | json '["result"]["value"]')
auth() { curl -sf -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
NAMESPACE=$(auth -X POST "$API/accounts/$ACCOUNT/storage/kv/namespaces" -d '{"title":"e2e"}' | json '["result"]["id"]')
SUBDOMAIN=$(auth "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')

APP="$WORK/kv-app"
cp -r "$E2E/fixtures/kv-app" "$APP"
render() { sed "s/__NAMESPACE__/$NAMESPACE/; s/__GREETING__/$1/" "$APP/wrangler.template.jsonc" > "$APP/wrangler.jsonc"; }
export CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT
export WRANGLER_SEND_METRICS=false CI=true
wrangler() { (cd "$APP" && "$E2E/node_modules/.bin/wrangler" "$@"); }
HOST="kv-app.$SUBDOMAIN.workers.test"
get() { curl -s -H "Host: $HOST" "http://127.0.0.1:$GATEWAY_PORT$1"; }
# A deploy reaches the runner within the agent's generation TTL (2 s).
settle() { sleep 2.5; }

render one
wrangler deploy > "$WORK/deploy1.log" 2>&1 || { cat "$WORK/deploy1.log"; exit 1; }
V1=$(grep -oE 'Current Version ID: [0-9a-f-]+' "$WORK/deploy1.log" | awk '{print $4}')
settle
check "deploy serves the first version" "one #1" "$(get /)"
check "KV state persists between requests" "one #2" "$(get /)"

render two
wrangler deploy > "$WORK/deploy2.log" 2>&1 || { cat "$WORK/deploy2.log"; exit 1; }
settle
check "redeploy rolls the process" "two #3" "$(get /)"

echo -n s3cret | wrangler secret put TOKEN > /dev/null 2>&1
settle
check "secret put reaches the worker" "s3cret" "$(get /secret)"

check "KV REST API reads the binding's writes" "3" \
  "$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/storage/kv/namespaces/$NAMESPACE/values/count")"
wrangler kv key put --namespace-id "$NAMESPACE" count 41 --remote > /dev/null 2>&1
check "wrangler kv key put is visible to the worker" "two #42" "$(get /)"

wrangler rollback "$V1" --message e2e -y > /dev/null 2>&1
settle
check "rollback serves the old version" "one #43" "$(get /)"

# R2: a bucket through wrangler, then the binding from a Worker.
wrangler r2 bucket create e2e-files > /dev/null 2>&1
echo -n "from wrangler" > "$WORK/obj.txt"
wrangler r2 object put e2e-files/seed.txt --file "$WORK/obj.txt" --remote > /dev/null 2>&1
curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/r2test" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"r2_bucket","name":"BUCKET","bucket_name":"e2e-files"}]};type=application/json' \
  -F "index.js=@$E2E/fixtures/r2.js;filename=index.js;type=application/javascript+module" > /dev/null
settle
R2_OUT=$(curl -s -H "Host: r2test.$SUBDOMAIN.workers.test" "http://127.0.0.1:$GATEWAY_PORT/run")
r2() { python3 -c "import sys, json; print(json.dumps(json.loads(sys.argv[1])$1, separators=(',', ':')))" "$R2_OUT"; }
check "R2 get with metadata" '"hello world"' "$(r2 '["get"]')"
check "R2 etag is the MD5" 'true' "$(r2 '["etagIsMd5"]')"
check "R2 ranges" '["world","rld"]' "[$(r2 '["range"]'),$(r2 '["suffix"]')]"
check "R2 list with a delimiter" '[["seed.txt","top.txt"],["a/","b/"]]' "[$(r2 '["list.objects"]'),$(r2 '["list.prefixes"]')]"
check "R2 pagination" '[["a/one.txt","a/two.txt"],true]' "$(r2 '["page1"]')"
check "R2 conditional puts" '[null,true,null]' "[$(r2 '["onlyIfMatchFails"]'),$(r2 '["onlyIfNoneMatchCreates"]'),$(r2 '["onlyIfNoneMatchBlocks"]')]"
check "R2 multipart upload" '[5242884,true,"aatail",true]' "[$(r2 '["multipart.size"]'),$(r2 '["multipart.etag"]'),$(r2 '["multipart.tail"]'),$(r2 '["multipart.minPartSize"]')]"
check "R2 object from wrangler is readable" "from wrangler" "$(wrangler r2 object get e2e-files/seed.txt --pipe --remote 2>/dev/null)"

wrangler delete > /dev/null 2>&1
settle
check "delete unroutes the hostname" "404" "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" "http://127.0.0.1:$GATEWAY_PORT/")"

echo "all single-node checks passed"
