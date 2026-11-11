#!/usr/bin/env bash
# Durable Object consistency under faults (RFC 0001, S1). Clients append to and
# read a few Durable Object logs (chaos/log.js) without pause while Chaos Mesh
# and kubectl break the runners one way after another; chaos/check.py then
# checks the whole history for linearizability: no acknowledged write lost, one
# order per object, no stale reads, no two owners at once.
#
# The phases, each under load:
#   baseline          no fault
#   runner-crash      SIGKILL a runner container (agent and workerd), 3 times:
#                     its objects wait for the 20 s cluster lease
#   workerd-kill      SIGKILL a runner's workerd processes, 4 times: the agent
#                     gives up their lease
#   store-partition   cut one runner off from the object store for 45 s, twice:
#                     its lease lapses while it still runs, and others claim
#                     its objects
#   store-slow        every runner's object store traffic delayed and lossy
#   peer-partition    cut one runner off from the other runners for 45 s
#   clock-ahead       one runner's clock 30 s ahead: it takes live peers for
#                     dead and claims their objects
#   clock-behind      one runner's clock 30 s behind: its own lease looks
#                     expired to everyone else
#   rolling-restart   kubectl rollout restart: graceful handoffs
#
# Expects the release `slv` in namespace `selvedge` (test/k8s/values.yaml plus
# chaos/values.yaml), chaos/nodeports.yaml applied, and Chaos Mesh installed,
# on the kind cluster `selvedge`. Leaves the history in $CHAOS_OUT (default: a
# temporary directory).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE="$ROOT/test/k8s/chaos"
NS=selvedge
K="kubectl -n $NS"
OUT=${CHAOS_OUT:-$(mktemp -d)}
mkdir -p "$OUT"
HISTORY="$OUT/history.jsonl"
: > "$HISTORY"
trap 'kill $(jobs -p) 2>/dev/null || true; $K delete networkchaos,timechaos,podchaos --all --wait=false > /dev/null 2>&1 || true' EXIT

json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
NODE=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' selvedge-control-plane)
API=http://$NODE:30787/client/v4
GW=$NODE:30080
for _ in $(seq 1 60); do curl -sf "http://$NODE:30787/healthz" > /dev/null && break; sleep 1; done

ADMIN=$($K get secret slv-secrets -o jsonpath='{.data.admin-token}' | base64 -d)
admin() { curl -sf -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' "$@"; }
ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"chaos"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"chaos"}' | json '["result"]["value"]')
SUBDOMAIN=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')
curl -sf -X PUT "$API/accounts/$ACCOUNT/workers/scripts/log" -H "Authorization: Bearer $TOKEN" \
  -F 'metadata={"main_module":"index.js","compatibility_date":"2026-10-01","bindings":[{"type":"durable_object_namespace","name":"LOG","class_name":"Log"}]};type=application/json' \
  -F "index.js=@$HERE/log.js;filename=index.js;type=application/javascript+module" > /dev/null
HOST="log.$SUBDOMAIN.workers.local"
for _ in $(seq 1 60); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" "http://$GW/?key=warmup&append=w")" = 200 ] && break
  sleep 1
done
echo "     account $ACCOUNT, gateway $GW"

runners() { $K get pods -l app.kubernetes.io/component=runner --field-selector=status.phase=Running -o name | sed 's|pod/||'; }
pick() { runners | shuf -n 1; }
others() { runners | grep -vx "$1"; }
settle() {
  $K rollout status deploy/slv-runner --timeout=300s > /dev/null
  $K wait --for=condition=Ready pod -l app.kubernetes.io/component=runner --timeout=300s > /dev/null
  sleep 5
}
epochs() { # ownership changes logged by workerd, where its logs say so
  $K logs -l app.kubernetes.io/component=runner -c runner --tail=-1 --max-log-requests=10 2>/dev/null \
    | grep -cE 'epoch [0-9]+ open' || true
}

# Chaos Mesh resources, removed after `duration` by deleting them.
chaos() { # kind name duration spec-yaml
  $K apply -f - > /dev/null <<EOF
apiVersion: chaos-mesh.org/v1alpha1
kind: $1
metadata: { name: $2 }
spec:
$4
EOF
  sleep "$3"
  $K delete "$1" "$2" --wait=true > /dev/null
}
pods_yaml() { # names... -> a selector listing those pods
  printf '    pods:\n      %s: [%s]\n' "$NS" "$(echo "$@" | tr ' ' ',')"
}

phase() { # name, then the fault's commands as a function name
  local name=$1 fault=$2 before
  before=$(epochs)
  echo "---- $name"
  rm -f "$OUT/stop"
  python3 "$HERE/workload.py" --addr "$GW" --host "$HOST" --phase "$name" \
    --stop "$OUT/stop" --out "$HISTORY" & local load=$!
  sleep 5
  "$fault"
  sleep 10
  touch "$OUT/stop"; wait $load
  settle
  echo "     workerd logged $(( $(epochs) - before )) ownership changes (best effort: logs of killed containers are gone)"
}

baseline() { sleep 20; }

runner_crash() {
  for i in 1 2 3; do
    local v; v=$(pick)
    echo "     SIGKILL runner container of $v"
    chaos PodChaos "crash-$i" 1 "  action: container-kill
  mode: all
  containerNames: [runner]
  selector:
$(pods_yaml "$v")"
    sleep 25
    $K wait --for=condition=Ready "pod/$v" --timeout=120s > /dev/null || true
  done
}

workerd_kill() {
  for _ in 1 2 3 4; do
    local v; v=$(pick)
    echo "     SIGKILL workerd in $v"
    $K exec "$v" -c runner -- sh -c \
      'for d in /proc/[0-9]*; do [ "$(cat $d/comm 2>/dev/null)" = workerd ] && kill -9 "${d#/proc/}"; done; true'
    sleep 15
  done
}

store_partition() {
  for i in 1 2; do
    local v; v=$(pick)
    echo "     partition $v from the object store for 45s"
    chaos NetworkChaos "store-partition-$i" 45 "  action: partition
  mode: all
  direction: both
  selector:
$(pods_yaml "$v")
  target:
    mode: all
    selector:
      labelSelectors: { app.kubernetes.io/component: seaweedfs }"
    sleep 20
  done
}

store_slow() {
  echo "     delay every runner's object store traffic by 100ms (+-50ms), 10% loss, for 60s"
  chaos NetworkChaos store-slow 60 "  action: netem
  mode: all
  direction: both
  delay: { latency: 100ms, jitter: 50ms }
  loss: { loss: '10' }
  selector:
    labelSelectors: { app.kubernetes.io/component: runner }
  target:
    mode: all
    selector:
      labelSelectors: { app.kubernetes.io/component: seaweedfs }"
}

peer_partition() {
  local v; v=$(pick)
  echo "     partition $v from the other runners for 45s"
  chaos NetworkChaos peer-partition 45 "  action: partition
  mode: all
  direction: both
  selector:
$(pods_yaml "$v")
  target:
    mode: all
    selector:
$(pods_yaml $(others "$v") | sed 's/^/  /')"
}

clock() { # offset
  local v; v=$(pick)
  echo "     $v's clock $1 for 60s"
  chaos TimeChaos "clock" 60 "  mode: all
  timeOffset: '$1'
  containerNames: [runner]
  selector:
$(pods_yaml "$v")"
}
clock_ahead() { clock +30s; }
clock_behind() { clock -30s; }

rolling_restart() {
  echo "     rolling restart of every runner"
  $K rollout restart deploy/slv-runner > /dev/null
  $K rollout status deploy/slv-runner --timeout=300s > /dev/null
}

phase baseline baseline
phase runner-crash runner_crash
phase workerd-kill workerd_kill
phase store-partition store_partition
phase store-slow store_slow
phase peer-partition peer_partition
phase clock-ahead clock_ahead
phase clock-behind clock_behind
phase rolling-restart rolling_restart

echo "---- check"
python3 "$HERE/check.py" "$HISTORY" --addr "$GW" --host "$HOST"
