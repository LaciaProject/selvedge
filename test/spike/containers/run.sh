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
# workerd reaches Docker through a recorder, which logs the API calls it makes.
REC=${XDG_RUNTIME_DIR:-/tmp}/spike-docker-$$.sock
python3 "$DIR/docker-recorder.py" "$REC" /var/run/docker.sock "$WORK/docker-api.jsonl" &
RPID=$!
for _ in $(seq 1 50); do [ -S "$REC" ] && break; sleep 0.1; done
cp "$DIR/worker.js" "$WORK/"
sed "s|unix:/var/run/docker.sock|unix:$REC|" "$DIR/config.capnp" > "$WORK/config.capnp"
"$SELVEDGE_WORKERD" serve "$WORK/config.capnp" > "$WORK/workerd.log" 2>&1 &
WPID=$!
trap 'kill $WPID $RPID 2>/dev/null; docker ps -aq --filter name=workerd | xargs -r docker rm -f > /dev/null 2>&1; rm -rf "$WORK" "$REC"' EXIT
for _ in $(seq 1 50); do curl -s -o /dev/null "$URL/" && break; sleep 0.2; done
# Containers' exits, with codes: AutoRemove deletes them, and their logs.
docker events --filter type=container --filter event=die --format '{{.Actor.Attributes.name}} {{.Actor.Attributes.image}} exit={{.Actor.Attributes.exitCode}}' > "$WORK/events.txt" &
EPID=$!

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
step egress_default "/out?name=a&u=http://example.com/"
step intercept "/intercept?name=a"
step cold_start_internet "/ready?name=net&internet=1"
step egress_http "/out?name=net&u=http://example.com/"
step egress_https "/out?name=net&u=https://example.com/"
step egress_private "/out?name=net&u=http://169.254.169.254/"
step second_container "/ready?name=b"
step destroy "/destroy?name=a"
step restart_after_destroy "/ready?name=a"
docker ps --format '{{.Names}} {{.Image}} {{.Status}}' > "$WORK/ps.txt"
kill $WPID 2>/dev/null; sleep 1; kill $RPID $EPID 2>/dev/null; sleep 0.5
# The sidecar on its own, with workerd's arguments, to see why it stops.
timeout 8 docker run --name spike-sidecar --cap-add NET_ADMIN --add-host host.docker.internal:host-gateway \
  cloudflare/proxy-everything:main --http-egress-port 18491 --http-ingress-address 0.0.0.0:39001 \
  --docker-gateway-cidr 172.17.0.0/16 --dns-enabled --tls-intercept --disable-ipv6 > "$WORK/sidecar.txt" 2>&1
echo "exit=$?" >> "$WORK/sidecar.txt"
docker rm -f spike-sidecar > /dev/null 2>&1
python3 - "$WORK/ps.txt" "$WORK/workerd.log" "${RUNTIME:-runc}" "$WORK/docker-api.jsonl" "$WORK/events.txt" "$WORK/sidecar.txt" "${RESULTS[@]}" <<'PY'
import json, os, sys
ps, log, runtime, api, events, sidecar, *steps = sys.argv[1:]
calls = []
if os.path.exists(api):
    for line in open(api):
        c = json.loads(line)
        s = f"{c['method']} {c['path']}"
        if "keys" in c:
            s += f" keys={','.join(c['keys'])}"
        if "HostConfig" in c:
            s += f" HostConfig={','.join(c['HostConfig'])}"
        if s not in calls:
            calls.append(s)
print(json.dumps({
    "runtime": runtime,
    "steps": [json.loads(s) for s in steps],
    "docker_api": calls,
    "container_exits": open(events).read().splitlines()[:20],
    "sidecar_alone": open(sidecar).read().splitlines()[-30:],
    "containers": open(ps).read().splitlines(),
    "workerd_log_tail": open(log).read().splitlines()[-25:],
}, indent=1))
PY
