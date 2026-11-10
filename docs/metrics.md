# Metrics

Every component serves Prometheus metrics (`GET /metrics`, plain HTTP) on `--metrics-listen`
(`SELVEDGE_METRICS_LISTEN`). The Helm chart turns this on by default (`metrics.enabled`, port 9464):
it adds `prometheus.io/*` pod annotations for Kubernetes service discovery, opens the port in the
NetworkPolicies (to `metrics.scrapeFrom`, or anywhere), and can create a `PodMonitor` for the
Prometheus Operator (`metrics.podMonitor.enabled`).

No metric carries an account or script id, so the number of series does not grow with tenants.
`selvedge all` serves every component's metrics on one port.

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `selvedge_http_requests_total` | counter | `component` (`gateway`, `agent`, `api`, `data`, `r2-s3`, `containers`), `code` (`2xx`, ...) | Requests served. Health checks are not counted. |
| `selvedge_http_request_duration_seconds` | histogram | `component` | Time from a request's arrival to its response headers. For the gateway and the agent this includes the Worker's own time. |
| `selvedge_gateway_runner_errors_total` | counter | `kind` | `unreachable`: no connection to a runner, so the gateway tried the next one. `failed`: the runner took the request and then failed (the client got 502). |
| `selvedge_gateway_healthy_runners` | gauge | | Runners passing the gateway's health checks. |
| `selvedge_connections_opened_total` | counter | `role` (the server's) | Connections opened to other components. Connections are kept and reused, so this grows far more slowly than requests. |
| `selvedge_agent_account_processes` | gauge | | Account workerd processes running on the runner. |
| `selvedge_agent_process_starts_total` | counter | | Account processes started: first use, a deploy, or after a stop. |
| `selvedge_agent_process_start_duration_seconds` | histogram | | From fetching an account's bundle to its sockets accepting. |
| `selvedge_agent_process_stops_total` | counter | `reason` | `replaced` (deploy), `idle`, `memory` (over its limit), `hung` (failed its health probe for `--hang-timeout`), `removed` (no scripts left), `shutdown`. |
| `selvedge_agent_process_exits_total` | counter | | Account processes that exited on their own: a crash, or the kernel's OOM killer in cgroup mode. |
| `selvedge_agent_cpu_throttles_total` | counter | | Times an account process over its CPU share was given a lower priority. |
| `selvedge_agent_binding_requests_total` | counter | `binding` (`kv`, `r2`, `queue`, `assets`, `other`), `code` (or `error`) | Binding calls the agent sent to the data service. Static assets served from the runner's cache are not counted. |
| `selvedge_agent_container_start_duration_seconds` | histogram | `result` (`ok`, `error`) | From workerd asking for a container to its application running: scheduling, image pull, sandbox boot. |
| `selvedge_agent_container_unsupported_total` | counter | `operation` (`snapshot`, `archive`, `image`, `exec`, `signal`, `container`, `other`) | Docker API calls from workerd that Selvedge answers with 501. A new operation here after a workerd upgrade means workerd uses something the shim does not implement. |
| `selvedge_agent_container_egress_refused_total` | counter | | Connections to a runner's container egress port from addresses that are not its instances. |
| `selvedge_container_instances` | gauge | `state` (`running`, `pending`) | Container instances, as the containers controller last listed them. |
| `selvedge_container_starts_total` | counter | `result` (`created`, `limit`, `error`) | Start requests: `limit` is a refusal by `max_instances` or an account or installation limit. |
| `selvedge_container_stops_total` | counter | `reason` | `destroyed`, `exited`, `failed`, `signal` (from workerd), `session` (the account process ended), `orphaned` (its runner is gone), `finished` and `stuck` (cleanup). |

Some useful alerts:

- `increase(selvedge_agent_process_stops_total{reason="hung"}[10m]) > 0`: a Worker is stuck in a loop.
- `increase(selvedge_agent_process_exits_total[10m]) > 0`: workerd crashed or was OOM-killed. Its Durable
  Objects were unavailable for up to 20 seconds.
- `selvedge_gateway_healthy_runners < <runner replicas>`: a runner is failing its health checks.
- A high share of 5xx in `selvedge_http_requests_total{component="gateway"}`.
- `increase(selvedge_container_starts_total{result="error"}[10m]) > 0`: instances fail to start (image
  pulls, scheduling, the sandbox); the containers controller's log says why.
- `increase(selvedge_agent_container_unsupported_total[1h]) > 0` after a workerd upgrade.
