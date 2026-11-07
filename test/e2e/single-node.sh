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
export SELVEDGE_R2_DEV_ZONE=r2.test
export SELVEDGE_STATE_DIR="$WORK/state"
export SELVEDGE_RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/slv-e2e-$$"
export SELVEDGE_API_LISTEN=127.0.0.1:$API_PORT
export SELVEDGE_DATA_LISTEN=127.0.0.1:$((PORT_BASE + 88))
export SELVEDGE_AGENT_LISTEN=127.0.0.1:$((PORT_BASE + 89))
export SELVEDGE_GATEWAY_LISTEN=127.0.0.1:$GATEWAY_PORT
S3_PORT=$((PORT_BASE + 90))
export SELVEDGE_R2_S3_LISTEN=127.0.0.1:$S3_PORT
# Collect unused static asset files within seconds, not a day.
export SELVEDGE_GC_INTERVAL_SECS=1 SELVEDGE_ASSET_GC_GRACE_SECS=2
# Per-account limits, low enough for a test to cross.
export SELVEDGE_ACCOUNT_MEMORY_MB=384 SELVEDGE_ACCOUNT_CPUS=0.5

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

# R2's S3-compatible API, through the official AWS SDK. Credentials as on
# Cloudflare: the token's id and the SHA-256 of its value.
S3_TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"s3"}')
S3_KEY=$(echo "$S3_TOKEN" | json '["result"]["s3"]["access_key_id"]')
S3_SECRET=$(echo "$S3_TOKEN" | json '["result"]["s3"]["secret_access_key"]')
s3_out=$(cd "$E2E" && node s3.mjs "http://127.0.0.1:$S3_PORT" "$S3_KEY" "$S3_SECRET" 2>&1) || true
echo "$s3_out" | sed 's/^/     /'
check "R2 S3 API: every AWS SDK check passes" "0" "$(echo "$s3_out" | grep -c '^FAIL' || true)"
check "R2 S3 API: the SDK ran its checks" "13" "$(echo "$s3_out" | grep -c '^ok' || true)"
(cd "$E2E" && node -e '
const { S3Client, PutObjectCommand } = require("@aws-sdk/client-s3");
const [endpoint, accessKeyId, secretAccessKey] = process.argv.slice(1);
new S3Client({ endpoint, region: "auto", forcePathStyle: true, credentials: { accessKeyId, secretAccessKey } })
  .send(new PutObjectCommand({ Bucket: "e2e-files", Key: "from-s3.txt", Body: "written over S3" }));' \
  "http://127.0.0.1:$S3_PORT" "$S3_KEY" "$S3_SECRET")
check "an object written over S3 is the binding's object" "written over S3" \
  "$(wrangler r2 object get e2e-files/from-s3.txt --pipe --remote 2>/dev/null)"

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

# D1 export and import through wrangler: a dump of one database restores into
# another, rows and blobs alike, and a failing import changes nothing.
d1q() { curl -sf -H "Authorization: Bearer $TOKEN" -X POST "$API/accounts/$ACCOUNT/d1/database/$1/query" -d "{\"sql\":\"$2\"}"; }
wrangler d1 export e2e-db --remote --output "$WORK/dump.sql" > "$WORK/export.log" 2>&1 || cat "$WORK/export.log"
check "wrangler d1 export writes the schema and the rows" "1 1" \
  "$(grep -c '^CREATE TABLE posts' "$WORK/dump.sql") $(grep -c '^INSERT INTO "posts"' "$WORK/dump.sql")"
wrangler d1 create e2e-copy > /dev/null 2>&1
COPY=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/d1/database?name=e2e-copy" | json '["result"][0]["uuid"]')
wrangler d1 execute e2e-copy --remote --file "$WORK/dump.sql" > "$WORK/import.log" 2>&1 || cat "$WORK/import.log"
check "wrangler d1 execute --file reports the queries it ran" "1" "$(grep -c 'Executed [0-9]* queries' "$WORK/import.log")"
wrangler d1 export e2e-copy --remote --output "$WORK/copy.sql" > /dev/null 2>&1
check "the imported copy exports the same dump" "same" "$(cmp -s "$WORK/dump.sql" "$WORK/copy.sql" && echo same || diff "$WORK/dump.sql" "$WORK/copy.sql" | head -5)"
printf "INSERT INTO posts (title) VALUES ('half');\nINSERT INTO missing_table VALUES (1);\n" > "$WORK/bad.sql"
wrangler d1 execute e2e-copy --remote --file "$WORK/bad.sql" > "$WORK/bad.log" 2>&1 || true
check "a failing import rolls back as a whole" "1 1" \
  "$(grep -ci 'missing_table' "$WORK/bad.log" | head -c1) $(d1q "$COPY" 'SELECT count(*) AS n FROM posts' | json '["result"][0]["results"][0]["n"]')"

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

# HTTP pull consumers: messages pushed, pulled, acked and retried over the API.
wrangler queues create e2e-pull > /dev/null 2>&1
wrangler queues consumer http add e2e-pull --batch-size 5 --message-retries 1 --visibility-timeout-secs 2 \
  > "$WORK/pull.log" 2>&1 || cat "$WORK/pull.log"
QID=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/queues?name=e2e-pull" | json '["result"][0]["queue_id"]')
qapi() { local path=$1; shift; curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "$API/accounts/$ACCOUNT/queues/$QID$path" "$@"; }
pulled() { python3 -c 'import sys, json; r = json.load(sys.stdin)["result"]; print("|".join(sorted(m["body"] + "#" + str(m["attempts"]) for m in r["messages"])))'; }
check "wrangler lists the HTTP pull consumer" "1" "$(wrangler queues consumer list e2e-pull 2>&1 | grep -c 'HTTP pull consumers')"
qapi /messages -X POST -d '{"body":{"n":1}}' > /dev/null
qapi /messages/batch -X POST -d '{"messages":[{"body":"two","content_type":"text"},{"body":{"n":3}}]}' > /dev/null
P1=$(qapi /messages/pull -X POST -d '{"batch_size":10}')
check "pull returns the pushed messages" 'two#1|{"n":1}#1|{"n":3}#1' "$(echo "$P1" | pulled)"
# Ack the JSON messages, retry the text one.
SETTLE=$(echo "$P1" | python3 -c '
import sys, json
m = json.load(sys.stdin)["result"]["messages"]
print(json.dumps({"acks": [{"lease_id": x["lease_id"]} for x in m if x["body"] != "two"],
                  "retries": [{"lease_id": x["lease_id"], "delay_seconds": 0} for x in m if x["body"] == "two"]}))')
check "acks and retries settle by lease id" "2 1" \
  "$(qapi /messages/ack -X POST -d "$SETTLE" | python3 -c 'import sys, json; r = json.load(sys.stdin)["result"]; print(r["ackCount"], r["retryCount"])')"
P2=$(qapi /messages/pull -X POST -d '{}')
check "a retried message comes back on the next pull" "two#2" "$(echo "$P2" | pulled)"
sleep 3
check "a lease past its visibility timeout no longer settles" "0" \
  "$(qapi /messages/ack -X POST -d "$(echo "$P2" | python3 -c 'import sys, json; print(json.dumps({"acks": [{"lease_id": m["lease_id"]} for m in json.load(sys.stdin)["result"]["messages"]]}))')" \
     | python3 -c 'import sys, json; print(json.load(sys.stdin)["result"]["ackCount"])')"
check "a message out of retries is dropped when its lease runs out" " 0" \
  "$(qapi /messages/pull -X POST -d '{}' | python3 -c 'import sys, json; r = json.load(sys.stdin)["result"]; print("|".join(m["body"] for m in r["messages"]), r["message_backlog_count"])')"
wrangler queues consumer http remove e2e-pull > /dev/null 2>&1
check "without an HTTP pull consumer, pulls are refused" "400" "$(qapi /messages/pull -X POST -d '{}' -o /dev/null -w '%{http_code}')"

# Queue settings: pausing delivery, a queue-wide delivery delay, purging.
wrangler queues create e2e-ctl > /dev/null 2>&1
wrangler queues consumer http add e2e-ctl > /dev/null 2>&1
QID=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/queues?name=e2e-ctl" | json '["result"][0]["queue_id"]')
count_pulled() { # pulls, acks what came, prints "<pulled> <backlog after>"
  local r; r=$(qapi /messages/pull -X POST -d '{"batch_size":10}')
  echo "$r" | python3 -c 'import sys, json; r = json.load(sys.stdin)["result"]; print(json.dumps({"acks": [{"lease_id": m["lease_id"]} for m in r["messages"]]}))' > "$WORK/acks.json"
  qapi /messages/ack -X POST -d @"$WORK/acks.json" > /dev/null
  echo "$(echo "$r" | python3 -c 'import sys, json; print(len(json.load(sys.stdin)["result"]["messages"]))') $(qapi '' | json '["result"]["settings"]["delivery_paused"]')"
}
wrangler queues pause-delivery e2e-ctl > /dev/null 2>&1
qapi /messages -X POST -d '{"body":"held"}' > /dev/null
check "a paused queue delivers nothing" "0 True" "$(count_pulled)"
wrangler queues resume-delivery e2e-ctl > /dev/null 2>&1
check "a resumed queue delivers what waited" "1 False" "$(count_pulled)"
wrangler queues update e2e-ctl --delivery-delay-secs 3 > /dev/null 2>&1
qapi /messages -X POST -d '{"body":"later"}' > /dev/null
check "the queue's delivery delay holds a message back" "0 False" "$(count_pulled)"
sleep 4
check "...and then delivers it" "1 False" "$(count_pulled)"
qapi /messages/batch -X POST -d '{"messages":[{"body":1,"delay_seconds":0},{"body":2,"delay_seconds":0}]}' > /dev/null
wrangler queues purge e2e-ctl --force > /dev/null 2>&1
check "purging empties the queue" "0 0" \
  "$(qapi /messages/pull -X POST -d '{}' | python3 -c 'import sys, json; r = json.load(sys.stdin)["result"]; print(len(r["messages"]), r["message_backlog_count"])')"

# R2 event notifications: object changes under a prefix go to a queue, which
# the queue app consumes alongside its own.
wrangler queues create e2e-events > /dev/null 2>&1
wrangler r2 bucket notification create e2e-files --event-type object-create object-delete \
  --queue e2e-events --prefix notif/ > "$WORK/notif.log" 2>&1 || cat "$WORK/notif.log"
check "wrangler lists the notification rule" "1" \
  "$(wrangler r2 bucket notification list e2e-files 2>&1 | grep -c 'notif/')"
sed -i 's/"consumers": \[/"consumers": [{ "queue": "e2e-events", "max_batch_size": 10 }, /' "$QAPP/wrangler.jsonc"
(cd "$QAPP" && "$E2E/node_modules/.bin/wrangler" deploy > "$WORK/queue.log" 2>&1) || { cat "$WORK/queue.log"; exit 1; }
settle
echo -n "hello" > "$WORK/notif.txt"
wrangler r2 object put e2e-files/notif/a.txt --file "$WORK/notif.txt" --remote > /dev/null 2>&1
wrangler r2 object put e2e-files/elsewhere.txt --file "$WORK/notif.txt" --remote > /dev/null 2>&1
(cd "$E2E" && node -e '
const { S3Client, CopyObjectCommand } = require("@aws-sdk/client-s3");
const [endpoint, accessKeyId, secretAccessKey] = process.argv.slice(1);
new S3Client({ endpoint, region: "auto", forcePathStyle: true, credentials: { accessKeyId, secretAccessKey } })
  .send(new CopyObjectCommand({ Bucket: "e2e-files", Key: "notif/copy.txt", CopySource: "e2e-files/seed.txt" }));' \
  "http://127.0.0.1:$S3_PORT" "$S3_KEY" "$S3_SECRET")
wrangler r2 object delete e2e-files/notif/a.txt --remote > /dev/null 2>&1
events() { # the e2e-events messages the consumer logged, one summary per line, sorted
  curl -s -H "Host: $QHOST" "http://127.0.0.1:$GATEWAY_PORT/" | python3 -c '
import sys, json
for entry in sys.stdin.read().split(";"):
    if not entry.startswith("e2e-events:"): continue
    m = json.loads(entry[len("e2e-events:"):].rsplit("#", 1)[0])
    o = m["object"]
    src = m.get("copySource")
    print(m["action"], m["bucket"], o["key"], o.get("size", "-"), "etag" if o.get("eTag") else "-",
          src["object"] if src else "-", "account" if m["account"] == sys.argv[1] else "?", "time" if m["eventTime"].endswith("Z") else "?")
' "$ACCOUNT" | sort
}
for _ in $(seq 1 30); do [ "$(events | wc -l)" -ge 3 ] && break; sleep 1; done
check "R2 notifications reach the queue, for matching keys only" \
  "CopyObject e2e-files notif/copy.txt 13 etag seed.txt account time|DeleteObject e2e-files notif/a.txt - - - account time|PutObject e2e-files notif/a.txt 5 etag - account time" \
  "$(events | paste -sd '|')"
wrangler r2 bucket notification delete e2e-files --queue e2e-events > /dev/null 2>&1
wrangler r2 object put e2e-files/notif/b.txt --file "$WORK/notif.txt" --remote > /dev/null 2>&1
sleep 4
check "a deleted notification rule sends nothing" "3" "$(events | wc -l)"

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
asset_dirs() { find "$WORK/blobs/assets/$ACCOUNT" -type f | wc -l; }
before=$(asset_dirs)
awrangler delete -c site.jsonc --force > /dev/null 2>&1 || awrangler delete -c site.jsonc > /dev/null 2>&1
after=$before
for _ in $(seq 1 30); do after=$(asset_dirs); [ "$after" -lt "$before" ] && break; sleep 0.5; done
check "deleting a site frees its asset files, and only those" "1" "$((before - after))"

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

# Zone routes: the operator assigns a zone; wrangler adds the routes.
check "a tenant cannot claim a zone" "403" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $TOKEN" "$API/zones" -d "{\"name\":\"stolen.test\",\"account\":{\"id\":\"$ACCOUNT\"}}")"
# Zone names are unique across the platform, and the test database is shared.
ZONE="r$(openssl rand -hex 4).test"
admin -X POST "$API/zones" -d "{\"name\":\"$ZONE\",\"account\":{\"id\":\"$ACCOUNT\"}}" > /dev/null
ROUTES="$WORK/route-app"
cp -r "$E2E/fixtures/route-app" "$ROUTES"
sed -i "s/routes\.test/$ZONE/g" "$ROUTES/wrangler.jsonc"
(cd "$ROUTES" && "$E2E/node_modules/.bin/wrangler" deploy > "$WORK/routes.log" 2>&1) || { cat "$WORK/routes.log"; exit 1; }
settle
rt() { curl -s -H "Host: $1" "http://127.0.0.1:$GATEWAY_PORT$2"; }
check "a route pattern reaches its Worker" "route:app.$ZONE/api/hello" "$(rt "app.$ZONE" /api/hello)"
check "a wildcard host route reaches its Worker" "route:x.$ZONE/wild/a" "$(rt "x.$ZONE" /wild/a)"
check "a path no route matches is not served" "404" \
  "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: app.$ZONE" "http://127.0.0.1:$GATEWAY_PORT/other")"

# Public buckets: the r2.dev-style URL, and a custom domain in the account's
# zone. Both read objects without credentials on the S3 listener.
ZONE_ID=$(admin "$API/zones?name=$ZONE" | json '["result"][0]["id"]')
code_as() { local host=$1 path=$2; shift 2; curl -s -o /dev/null -w '%{http_code}' -H "Host: $host" "$@" "http://127.0.0.1:$S3_PORT$path"; }
DEV=$(wrangler r2 bucket dev-url enable e2e-files -y 2>&1 | grep -o 'pub-[0-9a-f]*\.r2\.test' | head -1)
check "wrangler turns on a bucket's public URL" "pub-" "${DEV:0:4}"
check "the public URL serves an object without credentials" "from wrangler" \
  "$(curl -s -H "Host: $DEV" "http://127.0.0.1:$S3_PORT/seed.txt")"
check "the public URL serves ranges" "wrangler" \
  "$(curl -s -H "Host: $DEV" -H 'Range: bytes=5-' "http://127.0.0.1:$S3_PORT/seed.txt")"
ETAG=$(curl -s -I -H "Host: $DEV" "http://127.0.0.1:$S3_PORT/seed.txt" | tr -d '\r' | sed -n 's/^etag: //Ip')
check "the public URL answers conditional requests" "304" "$(code_as "$DEV" /seed.txt -H "If-None-Match: $ETAG")"
check "the public URL refuses writes" "405" "$(code_as "$DEV" /seed.txt -X PUT -d x)"
check "the public URL lists nothing" "404" "$(code_as "$DEV" /)"
check "the public URL answers 404 for a missing object" "404" "$(code_as "$DEV" /nope.txt)"
check "wrangler reports the public URL" "enabled at 'https://$DEV'" \
  "$(wrangler r2 bucket dev-url get e2e-files 2>&1 | grep -o "enabled at 'https://[^']*'")"
wrangler r2 bucket dev-url disable e2e-files -y > /dev/null 2>&1
check "a disabled public URL serves nothing" "403" "$(code_as "$DEV" /seed.txt)"
FILES="files.$ZONE"
wrangler r2 bucket domain add e2e-files --domain "$FILES" --zone-id "$ZONE_ID" -y > "$WORK/domain.log" 2>&1 || cat "$WORK/domain.log"
check "a custom domain serves the bucket" "from wrangler" "$(curl -s -H "Host: $FILES" "http://127.0.0.1:$S3_PORT/seed.txt")"
check "wrangler lists the custom domain" "$FILES" \
  "$(wrangler r2 bucket domain list e2e-files 2>&1 | grep -o "$FILES" | head -1)"
check "a custom domain outside the zone is refused" "1" \
  "$(wrangler r2 bucket domain add e2e-files --domain files.elsewhere.test --zone-id "$ZONE_ID" -y 2>&1 | grep -c 'is not in the zone')"
check "a custom domain in another account's zone is refused" "404" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    "$API/accounts/$ACCOUNT/r2/buckets/e2e-files/domains/custom" -d '{"domain":"x.y.test","zoneId":"0123456789abcdef0123456789abcdef"}')"
wrangler r2 bucket domain remove e2e-files --domain "$FILES" -y > /dev/null 2>&1
check "a removed custom domain serves nothing" "403" "$(code_as "$FILES" /seed.txt)"

# R2 lifecycle rules: wrangler sets them, the data service applies them.
r2code() { curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/r2/buckets/e2e-files/objects/$1"; }
check "a bucket starts with Cloudflare's default multipart rule" "Default Multipart Abort Rule" \
  "$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/r2/buckets/e2e-files/lifecycle" | json '["result"]["rules"][0]["id"]')"
echo -n "aging" > "$WORK/aging.txt"
wrangler r2 object put e2e-files/old/a.txt --file "$WORK/aging.txt" --remote > /dev/null 2>&1
wrangler r2 object put e2e-files/young/b.txt --file "$WORK/aging.txt" --remote > /dev/null 2>&1
UPLOAD=$(cd "$E2E" && node -e '
const { S3Client, CreateMultipartUploadCommand } = require("@aws-sdk/client-s3");
const [endpoint, accessKeyId, secretAccessKey] = process.argv.slice(1);
new S3Client({ endpoint, region: "auto", forcePathStyle: true, credentials: { accessKeyId, secretAccessKey } })
  .send(new CreateMultipartUploadCommand({ Bucket: "e2e-files", Key: "tmp/big.bin" })).then((r) => console.log(r.UploadId));' \
  "http://127.0.0.1:$S3_PORT" "$S3_KEY" "$S3_SECRET")
wrangler r2 bucket lifecycle add e2e-files expire-old old/ --expire-date 2020-01-01 -y > "$WORK/lc.log" 2>&1 || cat "$WORK/lc.log"
wrangler r2 bucket lifecycle add e2e-files abort-tmp tmp/ --abort-multipart-days 0 -y > "$WORK/lc.log" 2>&1 || cat "$WORK/lc.log"
check "wrangler lists the lifecycle rules" "2" \
  "$(wrangler r2 bucket lifecycle list e2e-files 2>&1 | grep -cE 'expire-old|abort-tmp')"
for _ in $(seq 1 20); do [ "$(r2code old/a.txt)" = 404 ] && break; sleep 0.5; done
check "lifecycle deletes objects past their date" "404" "$(r2code old/a.txt)"
check "...and keeps the others" "200" "$(r2code young/b.txt)"
check "lifecycle aborts stale multipart uploads" "NoSuchUpload" "$(cd "$E2E" && node -e '
const { S3Client, ListPartsCommand } = require("@aws-sdk/client-s3");
const [endpoint, accessKeyId, secretAccessKey, UploadId] = process.argv.slice(1);
new S3Client({ endpoint, region: "auto", forcePathStyle: true, credentials: { accessKeyId, secretAccessKey } })
  .send(new ListPartsCommand({ Bucket: "e2e-files", Key: "tmp/big.bin", UploadId })).then(() => console.log("still there"), (e) => console.log(e.name));' \
  "http://127.0.0.1:$S3_PORT" "$S3_KEY" "$S3_SECRET" "$UPLOAD")"
wrangler r2 bucket lifecycle remove e2e-files --name expire-old > /dev/null 2>&1
check "wrangler removes a lifecycle rule" "Default Multipart Abort Rule,abort-tmp" \
  "$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/r2/buckets/e2e-files/lifecycle" | python3 -c 'import sys, json; print(",".join(r["id"] for r in json.load(sys.stdin)["result"]["rules"]))')"

# Gradual deployments: two versions share the requests.
SPLIT="$WORK/split-app"
cp -r "$E2E/fixtures/split-app" "$SPLIT"
swr() { (cd "$SPLIT" && "$E2E/node_modules/.bin/wrangler" "$@"); }
swr deploy > "$WORK/split1.log" 2>&1 || { cat "$WORK/split1.log"; exit 1; }
V1=$(grep -o 'Current Version ID: [0-9a-f-]*' "$WORK/split1.log" | awk '{print $4}')
swr versions upload --var VERSION:two > "$WORK/split2.log" 2>&1 || { cat "$WORK/split2.log"; exit 1; }
V2=$(grep -o 'Worker Version ID: [0-9a-f-]*' "$WORK/split2.log" | awk '{print $4}')
swr versions deploy "$V1@50%" "$V2@50%" --yes > "$WORK/split3.log" 2>&1 || { cat "$WORK/split3.log"; exit 1; }
settle
sp() { curl -s -H "Host: split-app.$SUBDOMAIN.workers.test" "$@" "http://127.0.0.1:$GATEWAY_PORT/"; }
seen=$(for k in $(seq 1 40); do sp -H "Cloudflare-Workers-Version-Key: user-$k"; echo; done | sort -u | tr '\n' ' ')
check "a 50/50 deployment serves both versions" "one two " "$seen"
check "a version key keeps a client on one version" "1" \
  "$(for _ in 1 2 3 4 5; do sp -H 'Cloudflare-Workers-Version-Key: sticky'; echo; done | sort -u | wc -l)"
check "a version override picks the version" "two" "$(sp -H "Cloudflare-Workers-Version-Overrides: split-app=\"$V2\"")"
swr versions deploy "$V2@100%" --yes > /dev/null 2>&1
settle
check "deploying one version at 100% ends the split" "two two two" "$(for _ in 1 2 3; do sp; echo; done | tr '\n' ' ' | sed 's/ $//')"

# Per-account limits, on a second account so the first is not disturbed.
ACCOUNT2=$(admin -X POST "$API/accounts" -d '{"name":"hog"}' | json '["result"]["id"]')
TOKEN2=$(admin -X POST "$API/accounts/$ACCOUNT2/selvedge/tokens" -d '{"name":"hog"}' | json '["result"]["value"]')
SUB2=$(curl -sf -H "Authorization: Bearer $TOKEN2" "$API/accounts/$ACCOUNT2/workers/subdomain" | json '["result"]["subdomain"]')
curl -sf -X PUT "$API/accounts/$ACCOUNT2/workers/scripts/hog" -H "Authorization: Bearer $TOKEN2" \
  -F 'metadata={"main_module":"hog.js","compatibility_date":"2026-10-01"};type=application/json' \
  -F "hog.js=@$E2E/fixtures/hog.js;type=application/javascript+module" > /dev/null
settle
hog() { curl -s -m 10 -H "Host: hog.$SUB2.workers.test" "http://127.0.0.1:$GATEWAY_PORT$1"; }
hog / > /dev/null
check "a fresh account process counts from 1" "2" "$(hog /)"
for _ in $(seq 1 10); do [[ "$(hog /grow)" =~ ^[0-9]+$ ]] || break; sleep 0.3; done
sleep 3
check "a process over its memory limit is restarted" "1" "$(hog /)"
hog_pid() { pgrep -f "workerd serve .*/a/$ACCOUNT2/" | head -1; }
SPINNERS=()
for _ in 1 2 3; do (for _ in $(seq 1 12); do hog /spin > /dev/null; done) & SPINNERS+=($!); done
nice_value=""
for _ in $(seq 1 30); do
  nice_value=$(ps -o ni= -p "$(hog_pid)" 2>/dev/null | tr -d ' ')
  [ "$nice_value" = 10 ] && break
  sleep 0.5
done
wait "${SPINNERS[@]}"
check "a process over its CPU share is niced" "10" "$nice_value"

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
