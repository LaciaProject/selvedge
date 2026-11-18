#!/usr/bin/env bash
# Makes an account with something of everything the dashboard shows: a Worker
# (dash-app) with KV, D1, R2, a queue, a Durable Object and a Cron Trigger, a
# secret, two versions, a KV value and a D1 table. Prints the account id, its
# token and its workers.dev-style subdomain as shell assignments.
#   API=http://127.0.0.1:8787/client/v4 ADMIN_TOKEN=... WRANGLER=... dash/e2e/seed.sh [name]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
NAME=${1:-dash}
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
admin() { curl -sf -H "Authorization: Bearer $ADMIN_TOKEN" -H 'content-type: application/json' "$@"; }
ACCOUNT=$(admin -X POST "$API/accounts" -d "{\"name\":\"$NAME\"}" | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"seed"}' | json '["result"]["value"]')
auth() { curl -sf -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
KV=$(auth -X POST "$API/accounts/$ACCOUNT/storage/kv/namespaces" -d '{"title":"dash-kv"}' | json '["result"]["id"]')
D1=$(auth -X POST "$API/accounts/$ACCOUNT/d1/database" -d '{"name":"dash-db"}' | json '["result"]["uuid"]')
auth -X POST "$API/accounts/$ACCOUNT/r2/buckets" -d '{"name":"dash-bucket"}' > /dev/null
auth -X POST "$API/accounts/$ACCOUNT/queues" -d '{"queue_name":"dash-queue"}' > /dev/null
auth -X PUT "$API/accounts/$ACCOUNT/storage/kv/namespaces/$KV/values/greeting" -H 'content-type: text/plain' --data-binary 'hello' > /dev/null
# The first query starts the account's process: on a busy machine it may take
# a few tries. The statements may run again harmlessly.
SQL='{"sql":"CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes (body) SELECT b FROM (SELECT \"first\" AS b UNION ALL SELECT \"second\") WHERE NOT EXISTS (SELECT 1 FROM notes)"}'
for try in $(seq 1 10); do
  auth -X POST "$API/accounts/$ACCOUNT/d1/database/$D1/query" -d "$SQL" > /dev/null && break
  [ "$try" = 10 ] && { echo "the D1 database never answered" >&2; exit 1; }
  sleep 2
done

APP=$(mktemp -d)
trap 'rm -rf "$APP"' EXIT
cp "$HERE/fixture/index.js" "$APP/"
render() { sed "s/__KV__/$KV/; s/__D1__/$D1/; s/__GREETING__/$1/" "$HERE/fixture/wrangler.template.jsonc" > "$APP/wrangler.jsonc"; }
export CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT
export WRANGLER_SEND_METRICS=false CI=true
wrangler() { (cd "$APP" && "$WRANGLER" "$@"); }
render one && wrangler deploy > "$APP/deploy.log" 2>&1 || { cat "$APP/deploy.log" >&2; exit 1; }
render two && wrangler deploy > "$APP/deploy.log" 2>&1 || { cat "$APP/deploy.log" >&2; exit 1; }
echo -n s3cret | wrangler secret put TOKEN > /dev/null 2>&1
SUBDOMAIN=$(auth "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
echo "ACCOUNT=$ACCOUNT"
echo "TOKEN=$TOKEN"
echo "SUBDOMAIN=$SUBDOMAIN"
