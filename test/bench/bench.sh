#!/usr/bin/env bash
# Throughput and latency of the request path, with mTLS between components as
# Helm deploys them: client -> gateway -> runner (agent -> workerd) and, for
# bindings, -> data service. One runner, so results compare across commits on
# the same machine, not across machines.
#
# Needs SELVEDGE_DATABASE_URL, SELVEDGE_WORKERD and an S3 bucket with
# conditional writes (DO_S3_ENDPOINT, DO_S3_BUCKET) for the Durable Object
# scenario. Builds release binaries. Prints one JSON line per scenario, and
# writes them to $BENCH_OUT if set.
#
# BENCH_CONCURRENCY (16), BENCH_SECONDS (10) and BENCH_SCENARIOS
# ("plain kv do") tune the run.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
WORK=$(mktemp -d)
P=${PORT_BASE:-28500}
RUN=${XDG_RUNTIME_DIR:-/tmp}/slv-bench-$$
C=${BENCH_CONCURRENCY:-16}
SECS=${BENCH_SECONDS:-10}
SCENARIOS=${BENCH_SCENARIOS:-plain kv do}

# SELVEDGE_BIN measures another build (e.g. the base of a comparison) with this
# checkout's script, Worker and load generator.
if [ -n "${SELVEDGE_BIN:-}" ]; then
  (cd "$ROOT" && cargo build --release -q -p selvedge-bench)
  BIN=$SELVEDGE_BIN
else
  (cd "$ROOT" && cargo build --release -q -p selvedge -p selvedge-bench)
  BIN=$ROOT/target/release/selvedge
fi
LOAD=$ROOT/target/release/selvedge-bench

export SELVEDGE_BLOB_URL="file://$WORK/blobs"
export SELVEDGE_MASTER_KEY=$(openssl rand -hex 32)
export SELVEDGE_INTERNAL_TOKEN=$(openssl rand -hex 16)
export SELVEDGE_ADMIN_TOKEN=admin-$(openssl rand -hex 8)
export SELVEDGE_WORKERS_DEV_ZONE=workers.test
PUBLIC_API=http://127.0.0.1:$((P + 87))
export SELVEDGE_API_LISTEN=127.0.0.1:$((P + 87))
export SELVEDGE_DATA_LISTEN=127.0.0.1:$((P + 88)) SELVEDGE_DATA_ADDR=127.0.0.1:$((P + 88))
export SELVEDGE_API_INTERNAL_LISTEN=127.0.0.1:$((P + 86)) SELVEDGE_API_URL=https://127.0.0.1:$((P + 86))
export SELVEDGE_GATEWAY_INTERNAL_LISTEN=127.0.0.1:$((P + 79)) SELVEDGE_GATEWAY_URL=https://127.0.0.1:$((P + 79))
export SELVEDGE_DO_BUCKET="s3://${DO_S3_BUCKET:-selvedge-do}/bench-$$"
export SELVEDGE_DO_BUCKET_OPTIONS="aws_endpoint=${DO_S3_ENDPOINT:-http://127.0.0.1:29000},aws_region=us-east-1,aws_access_key_id=${DO_S3_KEY:-any},aws_secret_access_key=${DO_S3_SECRET:-any}"
export SELVEDGE_CLUSTER_SECRET=$(openssl rand -hex 32)
export RUST_LOG=${RUST_LOG:-warn}

# A platform CA and one certificate per role, as the Helm chart generates.
T="$WORK/tls"; mkdir -p "$T"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$T/ca.key" -out "$T/ca.pem" \
  -days 2 -subj /CN=selvedge-bench-ca 2>/dev/null
for role in api data gateway runner; do
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$T/$role.key" -out "$T/$role.csr" \
    -subj "/CN=$role" 2>/dev/null
  printf 'subjectAltName=DNS:%s.selvedge.internal\nextendedKeyUsage=serverAuth,clientAuth\n' "$role" > "$T/ext"
  openssl x509 -req -in "$T/$role.csr" -CA "$T/ca.pem" -CAkey "$T/ca.key" -CAcreateserial -out "$T/$role.pem" \
    -days 2 -extfile "$T/ext" 2>/dev/null
done
export SELVEDGE_TLS_CA="$T/ca.pem"
role() { echo "SELVEDGE_TLS_CERT=$T/$1.pem SELVEDGE_TLS_KEY=$T/$1.key"; }

PIDS=()
start() { local name=$1; shift; "$@" > "$WORK/$name.log" 2>&1 & PIDS+=($!); }
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  pkill -f "$WORK/" 2>/dev/null || true
  wait 2>/dev/null || true
  if [ "${FAILED:-0}" = 1 ]; then for f in "$WORK"/*.log; do echo "== $f" >&2; tail -30 "$f" >&2; done; fi
  rm -rf "$WORK" "$RUN"
}
trap 'FAILED=1' ERR
trap cleanup EXIT

start api env $(role api) "$BIN" api
start data env $(role data) "$BIN" data
for _ in $(seq 1 50); do curl -sf "$PUBLIC_API/healthz" >/dev/null && break; sleep 0.2; done
SELVEDGE_AGENT_LISTEN=127.0.0.1:$((P + 91)) SELVEDGE_STATE_DIR="$WORK/a1" SELVEDGE_RUN_DIR="$RUN/a1" \
  start a1 env $(role runner) "$BIN" agent
SELVEDGE_GATEWAY_LISTEN=127.0.0.1:$((P + 80)) SELVEDGE_RUNNERS=127.0.0.1:$((P + 91)) \
  start gw env $(role gateway) "$BIN" gateway
sleep 3

API=$PUBLIC_API/client/v4
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
admin() { curl -sf -H "Authorization: Bearer $SELVEDGE_ADMIN_TOKEN" -H 'content-type: application/json' "$@"; }
ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"bench"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"t"}' | json '["result"]["value"]')
auth() { curl -sf -H "Authorization: Bearer $TOKEN" "$@"; }
SUBDOMAIN=$(auth "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
NS=$(auth -H 'content-type: application/json' -X POST "$API/accounts/$ACCOUNT/storage/kv/namespaces" -d '{"title":"bench"}' | json '["result"]["id"]')
auth -X PUT "$API/accounts/$ACCOUNT/storage/kv/namespaces/$NS/values/k" --data-binary 'value' > /dev/null
auth -X PUT "$API/accounts/$ACCOUNT/workers/scripts/bench" \
  -F "metadata={\"main_module\":\"index.js\",\"compatibility_date\":\"2026-10-01\",\"bindings\":[{\"type\":\"kv_namespace\",\"name\":\"KV\",\"namespace_id\":\"$NS\"},{\"type\":\"durable_object_namespace\",\"name\":\"BENCH\",\"class_name\":\"Bench\"}]};type=application/json" \
  -F "index.js=@$ROOT/test/bench/worker.js;filename=index.js;type=application/javascript+module" > /dev/null
HOST="bench.$SUBDOMAIN.workers.test"
URL=http://127.0.0.1:$((P + 80))
for _ in $(seq 1 60); do [ "$(curl -s -H "Host: $HOST" "$URL/")" = ok ] && break; sleep 0.5; done
[ "$(curl -s -H "Host: $HOST" "$URL/kv")" = value ] || { echo "the KV scenario does not answer" >&2; FAILED=1; exit 1; }

for s in $SCENARIOS; do
  case $s in
    plain) path=/ ;;
    kv) path=/kv ;;
    do) path=/do ;;
    *) echo "unknown scenario $s" >&2; exit 1 ;;
  esac
  line=$("$LOAD" "$URL$path" --host "$HOST" -c "$C" -d "$SECS" --label "$s")
  echo "$line"
  [ -n "${BENCH_OUT:-}" ] && echo "$line" >> "$BENCH_OUT"
done
