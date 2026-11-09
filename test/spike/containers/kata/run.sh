#!/usr/bin/env bash
# Runs the Kata spike on a kind cluster that has kata-deploy installed. Prints
# a JSON object.
set -uo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)
# The probe pod runs the same steps as ../probe-netstack.sh.
script=$(sed -n "/^script='/,/^'$/p" "$DIR/../probe-netstack.sh" | sed '1s/^script=.//; $d')
kubectl create configmap probe --from-literal=script="$script
uname -r" --dry-run=client -o yaml | kubectl apply -f - > /dev/null
t0=$(date +%s)
kubectl apply -f "$DIR/pods.yaml" > /dev/null
ready=$(kubectl wait --for=condition=Ready pod/box --timeout=300s > /dev/null 2>&1 && echo yes || echo no)
t1=$(date +%s)
kubectl wait --for=condition=Ready pod/client --timeout=120s > /dev/null 2>&1
sleep 15  # does the sidecar stay up?
ip=$(kubectl get pod box -o jsonpath='{.status.podIP}')
ingress=$(kubectl exec client -- curl -s -m 10 -p -x "http://$ip:39001" --proxy-header "X-Dst-Addr: 127.0.0.1:8080" http://127.0.0.1:8080/ 2>&1)
kernel_box=$(kubectl exec box -c app -- uname -r 2>&1)
kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/probe --timeout=300s > /dev/null 2>&1
python3 - "$ready" "$((t1 - t0))" "$ingress" "$kernel_box" "$(uname -r)" \
  "$(kubectl get pod box -o json)" "$(kubectl logs box -c sidecar 2>&1 | tail -20)" "$(kubectl logs probe 2>&1)" <<'PY'
import json, sys
ready, secs, ingress, kbox, khost, box, sidecar_log, probe = sys.argv[1:]
box = json.loads(box)
statuses = {c["name"]: {"ready": c["ready"], "restarts": c["restartCount"], "state": list(c["state"])[0]}
            for c in box["status"].get("containerStatuses", [])}
steps = []
for line in probe.splitlines():
    parts = line.split("|", 2)
    if len(parts) == 3:
        steps.append({"step": parts[0], "exit": parts[1], "out": parts[2][:120]})
print(json.dumps({
    "pod_ready": ready, "pod_start_seconds": int(secs),
    "containers": statuses,
    "ingress_through_sidecar": ingress[:200],
    "guest_kernel": kbox, "host_kernel": khost,
    "sidecar_log": sidecar_log.splitlines(),
    "probe": steps, "probe_tail": probe.splitlines()[-2:],
}, indent=1))
PY
