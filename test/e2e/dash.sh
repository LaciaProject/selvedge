#!/usr/bin/env bash
# The web dashboard end to end (RFC 0003 §9): `selvedge all` with the
# dashboard's listener and Dex as the OpenID Connect provider, driven by
# Playwright in English and Chinese. Needs SELVEDGE_DATABASE_URL,
# SELVEDGE_WORKERD, Docker, wrangler (`npm install` in test/e2e), and in dash/
# `npm ci`, `npm run build` and `npx playwright install chromium`.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
E2E="$ROOT/test/e2e"
DASH="$ROOT/dash"
WORK=$(mktemp -d)
BIN=${SELVEDGE_BIN:-$ROOT/target/debug/selvedge}
PORT_BASE=${PORT_BASE:-28600}
API_PORT=$((PORT_BASE + 87)) GATEWAY_PORT=$((PORT_BASE + 80)) DASH_PORT=$((PORT_BASE + 91)) DEX_PORT=$((PORT_BASE + 56))
DEX="slv-dex-$$"

[ -f "$DASH/dist/index.html" ] || { echo "build the dashboard first: (cd dash && npm run build)" >&2; exit 1; }

export SELVEDGE_BLOB_URL="file://$WORK/blobs"
export SELVEDGE_MASTER_KEY=$(openssl rand -hex 32)
export SELVEDGE_INTERNAL_TOKEN=$(openssl rand -hex 16)
export SELVEDGE_ADMIN_TOKEN=admin-$(openssl rand -hex 8)
export SELVEDGE_WORKERS_DEV_ZONE=workers.test
export SELVEDGE_STATE_DIR="$WORK/state"
export SELVEDGE_RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/slv-dash-$$"
export SELVEDGE_API_LISTEN=127.0.0.1:$API_PORT
export SELVEDGE_DATA_LISTEN=127.0.0.1:$((PORT_BASE + 88))
export SELVEDGE_AGENT_LISTEN=127.0.0.1:$((PORT_BASE + 89))
export SELVEDGE_GATEWAY_LISTEN=127.0.0.1:$GATEWAY_PORT
export SELVEDGE_ACCOUNT_MEMORY_MB=0
# The dashboard, at its own origin, and single sign-on through Dex.
export SELVEDGE_DASH_LISTEN=127.0.0.1:$DASH_PORT
export SELVEDGE_DASH_URL=http://localhost:$DASH_PORT
export SELVEDGE_DASH_DIR="$DASH/dist"
export SELVEDGE_OIDC_ISSUER=http://127.0.0.1:$DEX_PORT/dex
export SELVEDGE_OIDC_CLIENT_ID=selvedge
export SELVEDGE_OIDC_CLIENT_SECRET=selvedge-e2e-secret
export SELVEDGE_OIDC_NAME=Dex
export SELVEDGE_OIDC_SCOPES=openid,email,profile,groups
export SELVEDGE_OIDC_ADMIN_GROUPS=platform-admins

sed "s/__DEX_PORT__/$DEX_PORT/; s/__DASH_PORT__/$DASH_PORT/" "$DASH/e2e/dex.template.yaml" > "$WORK/dex.yaml"
chmod 755 "$WORK" && chmod 644 "$WORK/dex.yaml"
docker run -d --name "$DEX" -p "127.0.0.1:$DEX_PORT:5556" -v "$WORK/dex.yaml:/etc/dex/config.yaml:ro" \
  ghcr.io/dexidp/dex:v2.46.1 dex serve /etc/dex/config.yaml > /dev/null

"$BIN" all > "$WORK/selvedge.log" 2>&1 &
PID=$!
cleanup() {
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  if [ "${KEEP_LOGS:-}" = 1 ] || [ "${FAILED:-0}" = 1 ]; then
    tail -80 "$WORK/selvedge.log"
    docker logs "$DEX" 2>&1 | tail -20
  fi
  docker rm -f "$DEX" > /dev/null 2>&1 || true
  rm -rf "$WORK" "$SELVEDGE_RUN_DIR"
}
trap 'FAILED=1' ERR
trap cleanup EXIT

for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$API_PORT/healthz" > /dev/null && break; sleep 0.5; done
for _ in $(seq 1 60); do curl -sf "$SELVEDGE_OIDC_ISSUER/.well-known/openid-configuration" > /dev/null && break; sleep 0.5; done

API=http://127.0.0.1:$API_PORT/client/v4
# An assignment, so that a failing seed stops here (an `eval "$(...)"` would not).
SEED=$(API=$API ADMIN_TOKEN=$SELVEDGE_ADMIN_TOKEN WRANGLER="$E2E/node_modules/.bin/wrangler" "$DASH/e2e/seed.sh" dash)
eval "$SEED"
echo "     seeded account $ACCOUNT"

cd "$DASH"
DASH_URL=$SELVEDGE_DASH_URL API_URL=$API ADMIN_TOKEN=$SELVEDGE_ADMIN_TOKEN ACCOUNT=$ACCOUNT TOKEN=$TOKEN \
  SUBDOMAIN=$SUBDOMAIN GATEWAY=http://127.0.0.1:$GATEWAY_PORT npx playwright test
