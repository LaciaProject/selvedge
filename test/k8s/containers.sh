#!/usr/bin/env bash
# Containers on Kubernetes (RFC 0002): instances are pods in Kata VMs, made by
# the containers controller. wrangler builds and pushes the image to the
# bundled registry and deploys the application; the node pulls it with the
# instance's own credentials. Expects the release `slv` in namespace
# `selvedge` installed with test/k8s/containers/values.yaml on the kind
# cluster of test/k8s/containers/kind.yaml. Prints timings for the summary.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
E2E="$ROOT/test/e2e"
NS=selvedge
K="kubectl -n $NS"
WORK=$(mktemp -d)
REGISTRY=localhost:30500
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK" "$DOCKER_CONFIG"' EXIT
export DOCKER_CONFIG=$(mktemp -d)

check() {
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; exit 1; fi
}
contains() {
  case "$3" in *"$2"*) echo "ok   $1" ;; *) echo "FAIL $1: expected [$2] in [$3]"; exit 1 ;; esac
}
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
since() { python3 -c "import time; print(round(time.time() - $1, 1))"; }
instances() { $K get pods -l selvedge.dev/component=container --no-headers -o custom-columns=:metadata.name 2>/dev/null; }
count() { instances | grep -c . || true; }
wait_count() { # n, seconds
  for _ in $(seq 1 $(($2 * 2))); do [ "$(count)" = "$1" ] && return; sleep 0.5; done
}

GW_PORT=18080
$K port-forward svc/slv-gateway $GW_PORT:80 > /dev/null 2>&1 &
for _ in $(seq 1 60); do curl -sf localhost:30787/healthz > /dev/null && break; sleep 1; done
for _ in $(seq 1 30); do curl -s -o /dev/null localhost:$GW_PORT/ && break; sleep 1; done

ADMIN=$($K get secret slv-secrets -o jsonpath='{.data.admin-token}' | base64 -d)
API=http://localhost:30787/client/v4
admin() { curl -sf -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' "$@"; }
ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"containers"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"k8s"}' | json '["result"]["value"]')
SUBDOMAIN=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')

APP="$WORK/container-app"
cp -r "$E2E/fixtures/container-app" "$APP"
cp "$APP/wrangler.template.jsonc" "$APP/wrangler.jsonc"
sed "s/__VERSION__/1/" "$E2E/fixtures/container-app/server.py" > "$APP/server.py"
export CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT
export CLOUDFLARE_CONTAINER_REGISTRY=$REGISTRY WRANGLER_SEND_METRICS=false CI=true XDG_CONFIG_HOME="$WORK/xdg"
T0=$(date +%s.%N)
(cd "$APP" && "$E2E/node_modules/.bin/wrangler" deploy > "$WORK/deploy.log" 2>&1) || { cat "$WORK/deploy.log"; exit 1; }
echo "     (wrangler deploy with build and push: $(since "$T0") s)"
check "the registry refuses anonymous pulls" "401" \
  "$(curl -s -o /dev/null -w '%{http_code}' "http://$REGISTRY/v2/$ACCOUNT/container-app-box/tags/list")"
# The node must pull the image itself, through the token service.
docker images -q "$REGISTRY/*/*" | sort -u | xargs -r docker rmi -f > /dev/null 2>&1 || true
sleep 3

HOST="container-app.$SUBDOMAIN.workers.local"
get() { curl -s -m 300 -H "Host: $HOST" "localhost:$GW_PORT$1"; }

# ---- An instance in its own VM.
T0=$(date +%s.%N)
check "start() runs an instance and its port answers" "hello one v1" "$(get '/start?name=a&msg=one')"
COLD_PULL=$(since "$T0")
echo "     (cold start with an image pull and a VM: $COLD_PULL s)"
POD=$(instances | head -1)
check "the instance is a pod in the Kata RuntimeClass" "kata-qemu" "$($K get pod "$POD" -o jsonpath='{.spec.runtimeClassName}')"
GUEST=$($K exec "$POD" -c app -- uname -r)
NODE=$(docker exec selvedge-control-plane uname -r)
[ "$GUEST" != "$NODE" ] && echo "ok   it runs its own kernel ($GUEST, the node runs $NODE)" || { echo "FAIL the instance shares the node's kernel ($GUEST)"; exit 1; }
check "it holds no service account token" "false" "$($K get pod "$POD" -o jsonpath='{.spec.automountServiceAccountToken}')"
check "its runner owns it" "Pod" "$($K get pod "$POD" -o jsonpath='{.metadata.ownerReferences[0].kind}')"
check "a second request reaches the same instance" "hello one v1" "$(get '/hello?name=a')"
check "the sidecar's CA is in the container" "present" "$(get '/ca?name=a')"
check "exec() returns stdout, stderr and the exit code" "[out] [err] 3" "$(get '/exec?name=a')"
check "exec() takes stdin" "ping 0" "$(get '/exec-stdin?name=a')"
check "exec() sets the environment and working directory" "hi there /tmp" "$(get '/exec-env?name=a')"
contains "an exec process can be killed" "killed 143" "$(get '/exec-kill?name=a')"
check "exec() can allocate a terminal" "terminal 0" "$(get '/exec-pty?name=a')"

# ---- Egress: through workerd only.
contains "outbound requests fail without enableInternet" "failed" "$(get '/out?name=a&u=http://example.com/')"
check "interceptOutboundHttp sends the container's request to the Worker" "200 intercepted /x" "$(get '/intercept?name=a')"
check "interceptOutboundHttps does the same for HTTPS, trusted through the sidecar's CA" "200 intercepted /y" "$(get '/intercept-https?name=a')"
T0=$(date +%s.%N)
check "a second object gets its own instance" "hello two v1" "$(get '/start?name=b&msg=two&internet=1')"
COLD=$(since "$T0")
echo "     (cold start, image on the node: $COLD s)"
contains "enableInternet lets the container out" "200 " "$(get '/out?name=b&u=http://example.com/')"
contains "workerd refuses the cluster's addresses" "failed" "$(get "/out?name=b&u=https://$(kubectl get svc kubernetes -o jsonpath='{.spec.clusterIP}')/")"
contains "and link-local ones" "failed" "$(get '/out?name=b&u=http://169.254.169.254/')"
check "two instances run" "2" "$(count)"

# ---- The NetworkPolicy: a pod labelled as an instance reaches runners' egress port and nothing else.
RUNNER_IP=$($K get pods -l app.kubernetes.io/component=runner -o jsonpath='{.items[0].status.podIP}')
API_IP=$($K get pods -l app.kubernetes.io/component=api -o jsonpath='{.items[0].status.podIP}')
# Whether a TCP port answers: curl fails to connect (7) or times out (28)
# when it does not.
reach='curl -s -o /dev/null -m 5 "http://$0/"; c=$?; if [ $c = 7 ] || [ $c = 28 ]; then echo "closed (curl exit $c)"; else echo "open (curl exit $c)"; fi'
probe() { # name, address
  $K run "$1" --restart=Never --image=curlimages/curl:8.11.1 -q \
    --labels=app.kubernetes.io/name=selvedge,app.kubernetes.io/instance=slv,app.kubernetes.io/component=container,selvedge.dev/probe=1 \
    --command -- sh -c "$reach" "$2" > /dev/null
  $K wait --for=jsonpath='{.status.phase}'=Succeeded "pod/$1" --timeout=90s > /dev/null 2>&1 || true
  local out; out=$($K logs "$1" 2>/dev/null | tail -1)
  echo "     ($1: $out)" >&2
  echo "${out%% *}"
  $K delete pod "$1" --wait=false > /dev/null 2>&1 || true
}
check "an instance reaches its runner's egress port" "open" \
  "$(probe np-runner "$RUNNER_IP:8791")"
check "but not the API" "closed" "$(probe np-api "$API_IP:8787")"
check "nor the internet directly" "closed" "$(probe np-net "1.1.1.1:443")"

# ---- Lifecycle.
check "destroy() removes the instance" "destroyed" "$(get '/destroy?name=b')"
wait_count 1 60
check "one instance is left" "1" "$(count)"
OWNER=$($K get pod "$(instances | head -1)" -o jsonpath='{.metadata.ownerReferences[0].name}')
$K delete pod "$OWNER" --wait=false > /dev/null
wait_count 0 120
check "an instance goes with its runner's pod" "0" "$(count)"
$K rollout status deploy/slv-runner --timeout=180s > /dev/null
sleep 3
T0=$(date +%s.%N)
check "the object starts its container again on the new runner" "hello again v1" "$(get '/start?name=a&msg=again')"
echo "     (restart on another runner: $(since "$T0") s)"

ID=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/containers/applications" | json '["result"][0]["id"]')
curl -sf -X DELETE -H "Authorization: Bearer $TOKEN" "$API/accounts/$ACCOUNT/containers/applications/$ID" > /dev/null
wait_count 0 120
check "deleting the application removes its instances" "0" "$(count)"
check "and their pull secrets" "0" "$($K get secrets -l selvedge.dev/component=container --no-headers 2>/dev/null | grep -c . || true)"
tags() {
  local p t
  p=$(curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -X POST \
    "$API/accounts/$ACCOUNT/containers/registries/$REGISTRY/credentials" -d '{"expiration_minutes": 5, "permissions": ["pull"]}' | json '["result"]["password"]')
  t=$(curl -s -u "v1:$p" "localhost:30787/registry/token?service=selvedge-registry&scope=repository:$ACCOUNT/container-app-box:pull" | json '["token"]')
  curl -s -H "Authorization: Bearer $t" "http://$REGISTRY/v2/$ACCOUNT/container-app-box/tags/list" | python3 -c "import sys, json; print(len(json.load(sys.stdin).get('tags') or []))"
}
for _ in $(seq 1 30); do [ "$(tags)" = 0 ] && break; sleep 1; done
check "and its images in the bundled registry" "0" "$(tags)"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### Containers on Kubernetes (Kata)"
    echo
    echo "| Cold start | Seconds |"
    echo "| --- | --- |"
    echo "| image pull + VM | $COLD_PULL |"
    echo "| VM, image on the node | $COLD |"
  } >> "$GITHUB_STEP_SUMMARY"
fi
echo "containers on kubernetes passed"
