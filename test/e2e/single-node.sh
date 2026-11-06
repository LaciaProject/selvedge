#!/usr/bin/env bash
# End-to-end test of the minimal profile: `selvedge all` on one node, driven by
# the official wrangler CLI. Needs SELVEDGE_DATABASE_URL and SELVEDGE_WORKERD;
# run `npm install` in test/e2e first.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
E2E="$ROOT/test/e2e"
WORK=$(mktemp -d)
# Account users (with --account-uids) must pass through to their directories.
[ -n "${SELVEDGE_ACCOUNT_UIDS:-}" ] && chmod 711 "$WORK"
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

# D1: a database through wrangler, migrations, then the binding from a Worker.
wrangler d1 create e2e-db > /dev/null 2>&1
DB=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/d1/database?name=e2e-db" | json '["result"][0]["uuid"]')
mkdir -p "$APP/migrations"
printf "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\nINSERT INTO posts (title) VALUES ('first');\n" > "$APP/migrations/0001_init.sql"
sed -i "s|\"kv_namespaces\"|\"d1_databases\": [{ \"binding\": \"DB\", \"database_name\": \"e2e-db\", \"database_id\": \"$DB\" }],\n  \"kv_namespaces\"|" "$APP/wrangler.jsonc"
wrangler d1 migrations apply e2e-db --remote > "$WORK/migrate.log" 2>&1 || { cat "$WORK/migrate.log"; exit 1; }
check "wrangler d1 execute sees the migration" "1" \
  "$(curl -sf -H "Authorization: Bearer $TOKEN" -X POST "$API/accounts/$ACCOUNT/d1/database/$DB/query" -d '{"sql":"SELECT count(*) AS n FROM posts"}' | json '["result"][0]["results"][0]["n"]')"
curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/d1test" -H "Authorization: Bearer $TOKEN" \
  -F "metadata={\"main_module\":\"index.js\",\"compatibility_date\":\"2026-10-01\",\"bindings\":[{\"type\":\"d1\",\"name\":\"DB\",\"id\":\"$DB\"}]};type=application/json" \
  -F "index.js=@$E2E/fixtures/d1.js;filename=index.js;type=application/javascript+module" > /dev/null
settle
check "D1 binding: prepare, bind, run, first, blobs, batch, all, raw, rollback, errors" \
  '{"insert":[true,1,true],"first":"ada","blob":[1,2,3],"batch":3,"all":["ada","grace","linus"],"raw":[["id","name"],[1,"ada"]],"rollback":true,"error":true}' \
  "$(curl -s -H "Host: d1test.$SUBDOMAIN.workers.test" "http://127.0.0.1:$GATEWAY_PORT/run")"

# Queues: a queue through wrangler, one Worker producing and consuming it.
wrangler queues create e2e-jobs > /dev/null 2>&1
QAPP="$WORK/queue-app"
mkdir -p "$QAPP" && cp "$E2E/fixtures/queue-app/index.js" "$QAPP/"
cat > "$QAPP/wrangler.jsonc" <<JSON
{ "name": "queue-app", "main": "index.js", "compatibility_date": "2026-10-01",
  "kv_namespaces": [{ "binding": "KV", "id": "$NAMESPACE" }],
  "queues": { "producers": [{ "binding": "JOBS", "queue": "e2e-jobs" }],
              "consumers": [{ "queue": "e2e-jobs", "max_batch_size": 5, "max_retries": 2 }] } }
JSON
(cd "$QAPP" && "$E2E/node_modules/.bin/wrangler" deploy > "$WORK/queue.log" 2>&1) || { cat "$WORK/queue.log"; exit 1; }
settle
QHOST="queue-app.$SUBDOMAIN.workers.test"
check "queue producer sends" "sent" "$(curl -s -H "Host: $QHOST" "http://127.0.0.1:$GATEWAY_PORT/send")"
q_out=""
for _ in $(seq 1 30); do
  q_out=$(curl -s -H "Host: $QHOST" "http://127.0.0.1:$GATEWAY_PORT/")
  [[ $q_out == *flaky* ]] && break
  sleep 1
done
check "queue consumer receives, acks and retries" \
  'e2e-jobs:{"n":1}#1;e2e-jobs:{"n":2}#1;e2e-jobs:"three"#1;e2e-jobs:{"flaky":true}#2;' "$q_out"

# Cron Triggers: deployed by wrangler, fired on the next minute boundary.
CRON="$WORK/cron-app"
mkdir -p "$CRON" && cp "$E2E/fixtures/cron-app/index.js" "$CRON/"
cat > "$CRON/wrangler.jsonc" <<JSON
{ "name": "cron-app", "main": "index.js", "compatibility_date": "2026-10-01",
  "kv_namespaces": [{ "binding": "KV", "id": "$NAMESPACE" }], "triggers": { "crons": ["* * * * *"] } }
JSON
(cd "$CRON" && "$E2E/node_modules/.bin/wrangler" deploy > "$WORK/cron.log" 2>&1) || { cat "$WORK/cron.log"; exit 1; }
cron_out=never
for _ in $(seq 1 70); do
  cron_out=$(curl -s -H "Host: cron-app.$SUBDOMAIN.workers.test" "http://127.0.0.1:$GATEWAY_PORT/")
  # Wait through "not routed yet" and "never": only a firing ends the loop.
  [[ $cron_out == *"|"* ]] && break
  sleep 1
done
check "a Cron Trigger runs the scheduled handler" "* * * * *|number" "$cron_out"

# Static Assets: a Worker with assets and run_worker_first rules, then a site
# with only assets.
ASSETS_APP="$WORK/assets-app"
cp -r "$E2E/fixtures/assets-app" "$ASSETS_APP"
awrangler() { (cd "$ASSETS_APP" && "$E2E/node_modules/.bin/wrangler" "$@"); }
awrangler deploy > "$WORK/assets.log" 2>&1 || { cat "$WORK/assets.log"; exit 1; }
check "wrangler uploads the assets" "Uploaded 3 files" "$(grep -o 'Uploaded [0-9]* files' "$WORK/assets.log" || cat "$WORK/assets.log")"
settle
AHOST="assets-app.$SUBDOMAIN.workers.test"
aget() { curl -s -H "Host: $AHOST" "http://127.0.0.1:$GATEWAY_PORT$1"; }
ahead() { curl -s -o /dev/null -w "$2" -H "Host: $AHOST" "http://127.0.0.1:$GATEWAY_PORT$1"; }
check "an asset is served before the Worker" "<h1>home</h1>" "$(aget /)"
check "assets get their content type" "text/css" "$(ahead /style.css '%{content_type}' | cut -d';' -f1)"
check "_headers applies" "assets" "$(curl -s -D - -o /dev/null -H "Host: $AHOST" "http://127.0.0.1:$GATEWAY_PORT/style.css" | tr -d '\r' | sed -n 's/^x-e2e: //Ip')"
check "html_handling adds the trailing slash" "307 /about/" "$(ahead /about '%{http_code} %{redirect_url}' | sed 's|http://[^/]*||')"
check "_redirects applies" "301" "$(ahead /old '%{http_code}')"
check "run_worker_first sends /api/* to the Worker" "worker:/api/hello" "$(aget /api/hello)"
check "the ASSETS binding fetches an asset" "asset:200:body { color: teal; }" "$(aget /api/asset)"
check "a path with no asset falls through to the Worker" "worker:/nothing" "$(aget /nothing)"
check "request.cf holds only what the gateway knows" '{"httpProtocol":"HTTP/1.1"}' "$(aget /api/cf)"
check "a client cannot set request.cf" '{"httpProtocol":"HTTP/1.1"}' \
  "$(curl -s -H "Host: $AHOST" -H 'x-selvedge-cf: {"country":"XX"}' "http://127.0.0.1:$GATEWAY_PORT/api/cf")"
# wrangler tail: console.log lines and exceptions of live requests.
# Its own process group: wrangler's launcher runs the CLI in a child process.
setsid bash -c 'cd "$1" && exec "$2" tail --format json' _ "$ASSETS_APP" "$E2E/node_modules/.bin/wrangler" \
  > "$WORK/tail.json" 2> "$WORK/tail.err" &
TAIL_PID=$!
for _ in $(seq 1 40); do
  aget /api/tailed > /dev/null; curl -s -o /dev/null -H "Host: $AHOST" "http://127.0.0.1:$GATEWAY_PORT/api/boom"
  grep -q 'boom from the worker' "$WORK/tail.json" 2>/dev/null && grep -q '/api/tailed' "$WORK/tail.json" && break
  sleep 0.5
done
kill -INT -- "-$TAIL_PID" 2>/dev/null || true
for _ in $(seq 1 40); do kill -0 "$TAIL_PID" 2>/dev/null || break; sleep 0.25; done
kill -KILL -- "-$TAIL_PID" 2>/dev/null || true
tail_summary=$(python3 - "$WORK/tail.json" <<'PY'
import json, sys
text = open(sys.argv[1]).read()
dec, i, logs, exc = json.JSONDecoder(), 0, False, False
while i < len(text):
    if text[i].isspace(): i += 1; continue
    e, i = dec.raw_decode(text, i)
    if e.get("scriptName") == "assets-app":
        logs |= ["handled", "/api/tailed"] in [l["message"] for l in e["logs"]]
        exc |= e["outcome"] == "exception" and any("boom from the worker" in x["message"] for x in e["exceptions"])
print(f"logs={logs} exceptions={exc}")
PY
)
[ "$tail_summary" = "logs=True exceptions=True" ] || { echo "== wrangler tail stderr"; cat "$WORK/tail.err"; echo "== stdout"; head -c 3000 "$WORK/tail.json"; }
check "wrangler tail streams logs and exceptions" "logs=True exceptions=True" "$tail_summary"
check "ending wrangler tail ends the session" "[]" \
  "$(auth "$API/accounts/$ACCOUNT/workers/scripts/assets-app/tails" | json '["result"]')"
awrangler deploy > "$WORK/assets2.log" 2>&1 || { cat "$WORK/assets2.log"; exit 1; }
check "a redeploy uploads nothing new" "No updated asset files" "$(grep -o 'No updated asset files' "$WORK/assets2.log" || cat "$WORK/assets2.log")"
awrangler deploy -c site.jsonc > "$WORK/site.log" 2>&1 || { cat "$WORK/site.log"; exit 1; }
settle
SHOST="static-site.$SUBDOMAIN.workers.test"
check "a Worker with only assets serves them" "<h1>spa</h1>" "$(curl -s -H "Host: $SHOST" "http://127.0.0.1:$GATEWAY_PORT/")"
check "single-page-application serves index.html for unknown paths" "<h1>spa</h1>" \
  "$(curl -s -H "Host: $SHOST" "http://127.0.0.1:$GATEWAY_PORT/deep/link")"

# Workflows: deployed by wrangler, driven through the binding and wrangler.
WFA="$WORK/workflow-app"
cp -r "$E2E/fixtures/workflow-app" "$WFA"
wwr() { (cd "$WFA" && "$E2E/node_modules/.bin/wrangler" "$@"); }
wwr deploy > "$WORK/wf.log" 2>&1 || { cat "$WORK/wf.log"; exit 1; }
settle
wf() { curl -s -H "Host: workflow-app.$SUBDOMAIN.workers.test" "http://127.0.0.1:$GATEWAY_PORT$1"; }
wf_status() { # instance id, wanted status: polls for up to 20 s
  local s=""
  for _ in $(seq 1 40); do
    s=$(auth "$API/accounts/$ACCOUNT/workflows/e2e-flow/instances/$1" 2>/dev/null | json '["result"]["status"]' 2>/dev/null)
    [ "$s" = "$2" ] && break
    sleep 0.5
  done
  echo "$s"
}
WID=$(wf "/start?n=21")
check "a workflow instance waits for its event" "waiting" \
  "$(for _ in $(seq 1 20); do [ "$(auth "$API/accounts/$ACCOUNT/workflows/e2e-flow/instances/$WID" | python3 -c 'import sys,json; s=json.load(sys.stdin)["result"]["steps"]; print("waiting" if any(x["type"]=="waitForEvent" and not x["finished"] for x in s) else "")')" = waiting ] && { echo waiting; break; }; sleep 0.5; done)"
wf "/approve?id=$WID" > /dev/null
check "the instance completes after its event" "complete" "$(wf_status "$WID" complete)"
check "the binding reports the output" '{"doubled": 42, "approved": true}' "$(wf "/status?id=$WID" | python3 -c 'import sys,json; print(json.dumps(json.load(sys.stdin)["output"]))')"
TID=$(wwr workflows trigger e2e-flow '{"n":5}' 2>&1 | grep -o '"[0-9a-f-]\{36\}"' | tr -d '"')
check "wrangler workflows trigger starts an instance" "36" "${#TID}"
wwr workflows instances send-event e2e-flow "$TID" --type approve --payload '{"ok":false}' > /dev/null 2>&1
check "wrangler workflows send-event completes it" "complete" "$(wf_status "$TID" complete)"
check "describe lists its steps" "double-1 approval-1 pause-1" \
  "$(auth "$API/accounts/$ACCOUNT/workflows/e2e-flow/instances/$TID" | python3 -c 'import sys,json; print(" ".join(x["name"] for x in json.load(sys.stdin)["result"]["steps"]))')"
check "wrangler workflows instances list sees both" "2" \
  "$(wwr workflows instances list e2e-flow --json 2>/dev/null | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')"

# Experimental workerd flags would be live in processes running with
# --experimental (cluster mode), so uploads may not use them.
printf 'export default { fetch() { return new Response("x"); } };' > "$WORK/exp.js"
check "an experimental compatibility flag is refused" "False" "$(curl -s -X PUT "$API/accounts/$ACCOUNT/workers/scripts/exp" \
  -H "Authorization: Bearer $TOKEN" -F 'metadata={"main_module":"exp.js","compatibility_date":"2026-10-01","compatibility_flags":["unsafe_module"]};type=application/json' \
  -F "exp.js=@$WORK/exp.js;type=application/javascript+module" | json '["success"]')"

# Per-account users (only when the agent runs as root with --account-uids).
if [ -n "${SELVEDGE_ACCOUNT_UIDS:-}" ]; then
  wuids=$(ps -o uid= -C workerd | tr -d ' ' | sort -u)
  check "account processes do not run as root" "" "$(echo "$wuids" | awk '$1 < 10000')"
  read -r bmode owner < <(stat -c '%a %u' "$SELVEDGE_RUN_DIR/${ACCOUNT:0:12}/b.sock")
  check "the binding socket is the account's alone" "600 yes" "$bmode $(echo "$wuids" | grep -qx "$owner" && echo yes || echo no)"
  check "the account's state directory is private" "700 $owner" "$(stat -c '%a %u' "$SELVEDGE_STATE_DIR/a/$ACCOUNT")"
fi

wrangler delete > /dev/null 2>&1
settle
check "delete unroutes the hostname" "404" "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" "http://127.0.0.1:$GATEWAY_PORT/")"

echo "all single-node checks passed"
