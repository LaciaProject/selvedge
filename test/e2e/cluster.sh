#!/usr/bin/env bash
# Durable Objects on two runners in workerd cluster mode, and failover when one
# runner dies. Needs SELVEDGE_DATABASE_URL, SELVEDGE_WORKERD and an S3 bucket
# with conditional writes at DO_S3_ENDPOINT (bucket DO_S3_BUCKET).
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
export SELVEDGE_API_LISTEN=127.0.0.1:$((P + 87)) SELVEDGE_API_URL=http://127.0.0.1:$((P + 87))
export SELVEDGE_DATA_LISTEN=127.0.0.1:$((P + 88)) SELVEDGE_DATA_ADDR=127.0.0.1:$((P + 88))
export SELVEDGE_DO_BUCKET="s3://${DO_S3_BUCKET:-selvedge-do}/cluster-$$"
export SELVEDGE_DO_BUCKET_OPTIONS="aws_endpoint=${DO_S3_ENDPOINT:-http://127.0.0.1:29000},aws_region=us-east-1,aws_access_key_id=${DO_S3_KEY:-any},aws_secret_access_key=${DO_S3_SECRET:-any}"
export SELVEDGE_CLUSTER_SECRET=$(openssl rand -hex 32)

PIDS=()
start() { # name, args...
  local name=$1; shift
  "$@" > "$WORK/$name.log" 2>&1 &
  PIDS+=($!)
  eval "PID_$name=$!"
}
agent() { # name, port
  SELVEDGE_AGENT_LISTEN=127.0.0.1:$2 SELVEDGE_STATE_DIR="$WORK/$1" SELVEDGE_RUN_DIR="$RUN/$1" \
    start "$1" "$BIN" agent
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

start api "$BIN" api
start data "$BIN" data
for _ in $(seq 1 50); do curl -sf "$SELVEDGE_API_URL/healthz" >/dev/null && break; sleep 0.2; done
agent a1 $((P + 91))
agent a2 $((P + 92))
SELVEDGE_GATEWAY_LISTEN=127.0.0.1:$((P + 80)) SELVEDGE_RUNNERS=127.0.0.1:$((P + 91)),127.0.0.1:$((P + 92)) \
  start gw "$BIN" gateway
sleep 3

API=$SELVEDGE_API_URL/client/v4
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
admin() { curl -sf -H "Authorization: Bearer $SELVEDGE_ADMIN_TOKEN" -H 'content-type: application/json' "$@"; }
check() {
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; FAILED=1; exit 1; fi
}
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
check "the first object kept its count" "11" "$(retry_get /)"
echo "all cluster checks passed"
