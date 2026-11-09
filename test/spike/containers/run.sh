#!/usr/bin/env bash
# One spike run against the local Docker daemon: builds the app image, starts
# workerd and records each step. Needs SELVEDGE_WORKERD and Docker. Prints a
# JSON object; RUNTIME labels it.
set -uo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
URL=http://127.0.0.1:18490
docker build -q -t selvedge-spike-app:1 "$DIR/app" > /dev/null
docker pull -q cloudflare/proxy-everything:main > /dev/null
cp "$DIR/config.capnp" "$DIR/worker.js" "$WORK/"
"$SELVEDGE_WORKERD" serve "$WORK/config.capnp" > "$WORK/workerd.log" 2>&1 &
WPID=$!
trap 'kill $WPID 2>/dev/null; docker ps -aq --filter name=workerd | xargs -r docker rm -f > /dev/null 2>&1; rm -rf "$WORK"' EXIT
for _ in $(seq 1 50); do curl -s -o /dev/null "$URL/" && break; sleep 0.2; done

step() { # key, path: records the answer and seconds taken
  local t0 t1 out
  t0=$(date +%s.%N)
  out=$(curl -s -m 90 "$URL$2" 2>&1 | head -c 300)
  t1=$(date +%s.%N)
  RESULTS+=("$(python3 -c 'import json,sys; print(json.dumps({"step": sys.argv[1], "seconds": round(float(sys.argv[3]) - float(sys.argv[2]), 3), "answer": sys.argv[4]}))' "$1" "$t0" "$t1" "$out")")
}
RESULTS=()
step cold_start "/ready?name=a"
step warm_hello "/hello?name=a"
step egress_http "/out?name=a&u=http://example.com/"
step egress_https "/out?name=a&u=https://example.com/"
step intercept "/intercept?name=a"
step second_container "/ready?name=b"
step destroy "/destroy?name=a"
step restart_after_destroy "/ready?name=a"
docker ps --format '{{.Names}} {{.Image}} {{.Status}}' > "$WORK/ps.txt"
python3 - "$WORK/ps.txt" "$WORK/workerd.log" "${RUNTIME:-runc}" "${RESULTS[@]}" <<'PY'
import json, sys
ps, log, runtime, *steps = sys.argv[1:]
print(json.dumps({
    "runtime": runtime,
    "steps": [json.loads(s) for s in steps],
    "containers": open(ps).read().splitlines(),
    "workerd_log_tail": open(log).read().splitlines()[-25:],
}, indent=1))
PY
