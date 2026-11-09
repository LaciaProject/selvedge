# Containers spike (RFC 0002 §9)

Runs workerd's own container backend (`containerEngine.localDocker`) against
Docker on a GitHub Actions runner, once per OCI runtime (runc, gVisor), and
records what works:

- `getTcpPort(8080).fetch()` into the container (ingress through the
  `proxy-everything` sidecar);
- the container's own `fetch` to the internet, which the sidecar must capture
  with iptables/TPROXY and send back through workerd (egress);
- `interceptOutboundHttp`, which routes a container's request to a Worker;
- cold and warm start times.

`run.sh` prints one JSON object. The `containers spike` workflow runs it for
each runtime and puts the results in its job summary. Throwaway code: what it
learns goes back into RFC 0002.
