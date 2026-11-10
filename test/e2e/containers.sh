#!/usr/bin/env bash
# End-to-end test of containers (RFC 0002) on one node: `selvedge all` with
# the Docker backend, a registry that authenticates through the API, and the
# official wrangler CLI building, pushing and deploying a container
# application. Needs SELVEDGE_DATABASE_URL, SELVEDGE_WORKERD and Docker; run
# `npm install` in test/e2e first.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
E2E="$ROOT/test/e2e"
WORK=$(mktemp -d)
BIN=${SELVEDGE_BIN:-$ROOT/target/debug/selvedge}
PORT_BASE=${PORT_BASE:-28900}
API_PORT=$((PORT_BASE + 87)) GATEWAY_PORT=$((PORT_BASE + 80)) REGISTRY_PORT=$((PORT_BASE + 50))
INSTANCE=slv-e2e-$$
REGISTRY=localhost:$REGISTRY_PORT

export SELVEDGE_BLOB_URL="file://$WORK/blobs"
export SELVEDGE_MASTER_KEY=$(openssl rand -hex 32)
export SELVEDGE_INTERNAL_TOKEN=$(openssl rand -hex 16)
export SELVEDGE_ADMIN_TOKEN=admin-$(openssl rand -hex 8)
export SELVEDGE_WORKERS_DEV_ZONE=workers.test
export SELVEDGE_STATE_DIR="$WORK/state"
export SELVEDGE_RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/slv-c-$$"
export SELVEDGE_API_LISTEN=127.0.0.1:$API_PORT
export SELVEDGE_DATA_LISTEN=127.0.0.1:$((PORT_BASE + 88))
export SELVEDGE_AGENT_LISTEN=127.0.0.1:$((PORT_BASE + 89))
export SELVEDGE_GATEWAY_LISTEN=127.0.0.1:$GATEWAY_PORT
export SELVEDGE_METRICS_LISTEN=127.0.0.1:$((PORT_BASE + 64))
# Containers: the registry the API is the token service of, and the local
# Docker daemon to run instances on.
openssl ecparam -name prime256v1 -genkey -noout -out "$WORK/registry.key" 2>/dev/null
openssl req -x509 -new -key "$WORK/registry.key" -days 2 -subj "/CN=selvedge registry tokens" -out "$WORK/registry.crt" 2>/dev/null
export SELVEDGE_REGISTRY_HOST=$REGISTRY
export SELVEDGE_REGISTRY_TOKEN_KEY="$WORK/registry.key" SELVEDGE_REGISTRY_TOKEN_CERT="$WORK/registry.crt"
export SELVEDGE_CONTAINERS_DOCKER_SOCKET=${DOCKER_SOCKET:-/var/run/docker.sock}
export SELVEDGE_CONTAINERS_LISTEN=127.0.0.1:$((PORT_BASE + 92))
export SELVEDGE_CONTAINER_EGRESS_LISTEN=0.0.0.0:$((PORT_BASE + 91))
export SELVEDGE_INSTANCE=$INSTANCE
export SELVEDGE_CONTAINERS_ACCOUNT_INSTANCES=5

docker run -d --name "$INSTANCE-registry" -p "127.0.0.1:$REGISTRY_PORT:5000" \
  -v "$WORK/registry.crt:/token.crt:ro" \
  -e REGISTRY_AUTH=token \
  -e REGISTRY_AUTH_TOKEN_REALM="http://127.0.0.1:$API_PORT/registry/token" \
  -e REGISTRY_AUTH_TOKEN_SERVICE=selvedge-registry \
  -e REGISTRY_AUTH_TOKEN_ISSUER=selvedge \
  -e REGISTRY_AUTH_TOKEN_ROOTCERTBUNDLE=/token.crt \
  registry:2 > /dev/null

"$BIN" all > "$WORK/selvedge.log" 2>&1 &
PID=$!
instances() { docker ps -aq --filter "label=app.kubernetes.io/instance=$INSTANCE" --filter "label=selvedge.dev/role=${1:-app}"; }
cleanup() {
  [ -n "${LOG_COPY:-}" ] && cp "$WORK/selvedge.log" "$LOG_COPY" 2>/dev/null || true
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  if [ "${KEEP_LOGS:-}" = 1 ] || [ "${FAILED:-0}" = 1 ]; then
    tail -60 "$WORK/selvedge.log"
    for f in "$WORK"/deploy*.log; do [ -f "$f" ] && tail -30 "$f"; done
  fi
  docker ps -aq --filter "label=app.kubernetes.io/instance=$INSTANCE" | xargs -r docker rm -f > /dev/null 2>&1 || true
  docker rm -f -v "$INSTANCE-registry" > /dev/null 2>&1 || true
  docker images -q "$REGISTRY/*/*" | sort -u | xargs -r docker rmi -f > /dev/null 2>&1 || true
  rm -rf "$WORK" "$SELVEDGE_RUN_DIR" "$DOCKER_CONFIG"
}
trap 'FAILED=1' ERR
trap cleanup EXIT
# `docker login` writes here, not into the user's own configuration.
export DOCKER_CONFIG=$(mktemp -d)

API=http://127.0.0.1:$API_PORT/client/v4
for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:$API_PORT/healthz" >/dev/null && break; sleep 0.2; done

json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }
admin() { curl -sf -H "Authorization: Bearer $SELVEDGE_ADMIN_TOKEN" -H 'content-type: application/json' "$@"; }
check() { # description, expected, actual
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; FAILED=1; exit 1; fi
}
contains() { # description, expected part, actual
  case "$3" in *"$2"*) echo "ok   $1" ;; *) echo "FAIL $1: expected [$2] in [$3]"; FAILED=1; exit 1 ;; esac
}

ACCOUNT=$(admin -X POST "$API/accounts" -d '{"name":"containers"}' | json '["result"]["id"]')
TOKEN=$(admin -X POST "$API/accounts/$ACCOUNT/selvedge/tokens" -d '{"name":"e2e"}' | json '["result"]["value"]')
OTHER=$(admin -X POST "$API/accounts" -d '{"name":"other"}' | json '["result"]["id"]')
auth() { curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
SUBDOMAIN=$(auth "$API/accounts/$ACCOUNT/workers/subdomain" | json '["result"]["subdomain"]')

APP="$WORK/container-app"
cp -r "$E2E/fixtures/container-app" "$APP"
cp "$APP/wrangler.template.jsonc" "$APP/wrangler.jsonc"
version() { sed "s/__VERSION__/$1/" "$E2E/fixtures/container-app/server.py" > "$APP/server.py"; }
export CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT
export CLOUDFLARE_CONTAINER_REGISTRY=$REGISTRY
export WRANGLER_SEND_METRICS=false CI=true
# wrangler checks a stored OAuth login's scopes even when a token is set: a
# developer's own login must not decide this test.
export XDG_CONFIG_HOME="$WORK/xdg"
wrangler() { (cd "$APP" && "$E2E/node_modules/.bin/wrangler" "$@"); }
HOST="container-app.$SUBDOMAIN.workers.test"
get() { curl -s -m 120 -H "Host: $HOST" "http://127.0.0.1:$GATEWAY_PORT$1"; }
settle() { sleep 2.5; }

# ---- The registry: credentials cover the account's own repositories only.
CRED=$(auth -X POST "$API/accounts/$ACCOUNT/containers/registries/$REGISTRY/credentials" \
  -d '{"expiration_minutes": 5, "permissions": ["pull"]}')
PASSWORD=$(echo "$CRED" | json '["result"]["password"]')
scopes() { # scope -> the actions a token grants on it
  curl -s -u "v1:$PASSWORD" "http://127.0.0.1:$API_PORT/registry/token?service=selvedge-registry&scope=$1" |
    python3 -c "
import sys, json, base64
t = json.load(sys.stdin)['token'].split('.')[1]
print(json.loads(base64.urlsafe_b64decode(t + '=' * (-len(t) % 4)))['access'])"
}
check "a pull credential grants pull on the account's repository" \
  "[{'actions': ['pull'], 'name': '$ACCOUNT/app', 'type': 'repository'}]" \
  "$(scopes "repository:$ACCOUNT/app:pull,push" | python3 -c "import sys, ast; print([dict(sorted(d.items())) for d in ast.literal_eval(sys.stdin.read())])")"
check "and nothing on another account's" "[]" "$(scopes "repository:$OTHER/app:pull")"
check "the token service refuses a bad password" "401" \
  "$(curl -s -o /dev/null -w '%{http_code}' -u v1:nope "http://127.0.0.1:$API_PORT/registry/token?service=selvedge-registry")"
check "the registry refuses anonymous pulls" "401" \
  "$(curl -s -o /dev/null -w '%{http_code}' "http://$REGISTRY/v2/$ACCOUNT/app/tags/list")"

# ---- Deploy: wrangler builds the image, pushes it and creates the application.
version 1
wrangler deploy > "$WORK/deploy1.log" 2>&1 || { cat "$WORK/deploy1.log"; exit 1; }
APPS=$(auth "$API/accounts/$ACCOUNT/containers/applications")
check "the application exists" "container-app-box 2" "$(echo "$APPS" | json '["result"][0]["name"]') $(echo "$APPS" | json '["result"][0]["max_instances"]')"
IMAGE=$(echo "$APPS" | json '["result"][0]["configuration"]["image"]')
contains "its image is in the account's registry namespace" "$REGISTRY/$ACCOUNT/container-app-box" "$IMAGE"
# So that the first start has to pull, with the controller's credentials.
docker images -q "$REGISTRY/*/*" | sort -u | xargs -r docker rmi -f > /dev/null 2>&1 || true
settle

# ---- An instance: start, reach its port, environment, the CA.
T0=$(date +%s.%N)
check "start() runs the container and its port answers" "hello one v1" "$(get '/start?name=a&msg=one')"
echo "     (cold start with an image pull: $(python3 -c "import time; print(round(time.time() - $T0, 1))") s)"
check "a second request reaches the same instance" "hello one v1" "$(get '/hello?name=a')"
check "ctx.container.running" "true" "$(get '/running?name=a')"
check "the sidecar's CA is in the container" "present" "$(get '/ca?name=a')"
check "one instance runs" "1" "$(instances | wc -l | tr -d ' ')"

# ---- Egress: nothing by default, interception, the internet when asked for.
contains "outbound requests fail without enableInternet" "failed" "$(get '/out?name=a&u=http://example.com/')"
check "interceptOutboundHttp sends the container's request to the Worker" "200 intercepted /x" "$(get '/intercept?name=a')"
check "interceptOutboundHttps does the same for HTTPS, trusted through the sidecar's CA" "200 intercepted /y" "$(get '/intercept-https?name=a')"
T0=$(date +%s.%N)
check "a second object gets its own instance, with an entrypoint" "hello two v1" "$(get '/start?name=b&msg=two&internet=1&args=extra')"
echo "     (cold start, image present: $(python3 -c "import time; print(round(time.time() - $T0, 1))") s)"
check "start()'s entrypoint replaced the command" "extra" "$(get '/args?name=b')"
if curl -s -m 5 -o /dev/null http://example.com/; then
  contains "enableInternet lets the container out" "200 " "$(get '/out?name=b&u=http://example.com/')"
  contains "workerd refuses link-local addresses" "failed" "$(get '/out?name=b&u=http://169.254.169.254/')"
else
  echo "skip enableInternet (this machine has no route to example.com)"
fi
check "the first object is untouched" "hello one v1" "$(get '/hello?name=a')"

# ---- Limits and refusals.
# workerd tells the Worker only that the start failed; the agent's log says why.
contains "max_instances stops a third instance" "failed to start" "$(get '/start?name=c&msg=three')"
contains "and the log says why" "already runs its maximum of 2 instances" "$(cat "$WORK/selvedge.log")"
contains "start() with another image is refused" "error" "$(get '/start?name=d&msg=x&image=docker.io/library/alpine:3.20')"
check "and started nothing" "2" "$(instances | wc -l | tr -d ' ')"
contains "exec() fails with a clear message" "does not support" "$(get '/exec?name=a')"

# ---- Exit codes, destroy, restart.
contains "monitor() reports the exit code" "3" "$(get '/exit?name=b&code=3')"
sleep 1
check "the exited container is no longer running" "false" "$(get '/running?name=b')"
for _ in $(seq 1 30); do [ "$(instances | wc -l)" = 1 ] && break; sleep 0.5; done
check "its instance is removed" "1" "$(instances | wc -l | tr -d ' ')"
check "it starts again" "hello again v1" "$(get '/start?name=b&msg=again')"
check "destroy() removes the instance" "destroyed" "$(get '/destroy?name=b')"
check "one instance is left" "1 1" "$(instances | wc -l | tr -d ' ') $(instances proxy | wc -l | tr -d ' ')"

# ---- A new image: the application rolls out and instances start with it.
version 2
wrangler deploy > "$WORK/deploy2.log" 2>&1 || { cat "$WORK/deploy2.log"; exit 1; }
APPS=$(auth "$API/accounts/$ACCOUNT/containers/applications")
check "the application's image changed" "1 true" \
  "$(echo "$APPS" | json '["result"].__len__()') $([ "$(echo "$APPS" | json '["result"][0]["configuration"]["image"]')" != "$IMAGE" ] && echo true)"
settle
check "the new image runs after the rollout" "hello new v2" "$(get '/start?name=a&msg=new')"
for _ in $(seq 1 40); do [ "$(instances | wc -l)" = 1 ] && break; sleep 0.5; done
check "the old process's instances are gone" "1" "$(instances | wc -l | tr -d ' ')"

# ---- wrangler deploy with nothing changed creates no rollout.
wrangler deploy > "$WORK/deploy3.log" 2>&1 || { cat "$WORK/deploy3.log"; exit 1; }
contains "an unchanged deploy changes nothing" "no changes" "$(tr 'A-Z' 'a-z' < "$WORK/deploy3.log")"

# ---- Deleting the application takes ctx.container away.
ID=$(auth "$API/accounts/$ACCOUNT/containers/applications" | json '["result"][0]["id"]')
check "the application can be deleted" "True" "$(auth -X DELETE "$API/accounts/$ACCOUNT/containers/applications/$ID" | json '["success"]')"
for _ in $(seq 1 40); do [ "$(instances | wc -l)" = 0 ] && break; sleep 0.5; done
check "its instances are removed" "0" "$(instances | wc -l | tr -d ' ')"

METRICS=$(curl -s "http://$SELVEDGE_METRICS_LISTEN/metrics")
contains "container starts are counted" 'selvedge_container_starts_total{result="created"}' "$METRICS"
contains "refused starts are counted" 'selvedge_container_starts_total{result="limit"}' "$METRICS"
contains "unsupported calls are counted" 'selvedge_agent_container_unsupported_total{operation="exec"}' "$METRICS"
echo "containers e2e passed"
