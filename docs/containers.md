# Running containers

Durable Objects can have containers, as on Cloudflare (see [compatibility.md](compatibility.md#containers)
for what tenants get). This page is for operators. The design is in
[RFC 0002](rfcs/0002-containers.md).

## How it works

```
wrangler ── push ──▶ registry ◀── token ── API (/registry/token)
   │                    ▲
   └── deploy ──▶ API   │ pull (per-instance, pull-only credential)
                        │
runner pod                              instance pod (RuntimeClass kata-qemu)
┌───────────────────────────────┐       ┌─────────────────────────────────┐
│ workerd (account A)           │       │ proxy-everything sidecar         │
│   └ Docker API ─▶ agent shim  │──────▶│ application (A's image)          │
│ agent: ingress forward,       │◀──────│ all egress, via the sidecar      │
│        egress relay (:8791)   │       └─────────────────────────────────┘
└──────────────┬────────────────┘                   ▲
               │ mTLS (role: containers)            │ creates, deletes
               ▼                                    │
        containers controller (the only component with cluster credentials)
```

- workerd's built-in container support speaks the Docker Engine API. Each account process gets a
  socket of the agent's instead of Docker's; the agent passes on only the class, environment and
  command.
- The **containers controller** (`selvedge containers`) builds each instance's pod itself from the
  application in PostgreSQL: the image pinned by digest, its resources, the sandbox, a pull-only
  credential for the account's repositories. A compromised runner can start nothing but its
  accounts' applications, within their limits.
- Instances are ordinary pods in the release's namespace, owned by the runner pod that asked for
  them: if the runner goes, Kubernetes removes them. Their labels are the only state; the
  controller derives everything from listing them.
- `exec()` runs through the controller too: the agent opens an upgraded mTLS connection to it for
  each command, and the controller runs the command with Docker's exec or the pod's `exec`
  subresource. Runners never reach an instance's runtime.
- All of an instance's outbound traffic goes through its sidecar to the agent's egress relay, which
  hands it to the owning account's workerd. workerd applies the object's rules: nothing by
  default, public addresses with `enableInternet`, and `interceptOutboundHttp` handlers.

## Requirements

- **A sandbox.** By default instances run in the `kata-qemu` RuntimeClass, each in its own VM.
  Install Kata Containers on the nodes that will run instances (for example with
  [kata-deploy](https://github.com/kata-containers/kata-containers/tree/main/tools/packaging/kata-deploy)).
  Those nodes need KVM: bare metal, or cloud VMs with nested virtualization. Keep instances on them
  with `containers.podTemplate`, e.g. `{ nodeSelector: { katacontainers.io/kata-runtime: "true" } }`.
  gVisor does not work: workerd's sidecar needs policy routing and TPROXY, which gVisor lacks.
- **An image registry** reachable under one name both by clients (`wrangler deploy` pushes there)
  and by every node (the kubelet pulls from there), with TLS the nodes trust. The chart bundles
  one (CNCF distribution); expose its Service under that name yourself, e.g. with an Ingress or a
  LoadBalancer and a certificate. Or use your own registry with Docker's token authentication
  (below).
- **The API's token endpoint**, `<API URL>/registry/token`, reachable by clients and nodes: the
  registry sends both there for a token.
- Kubernetes 1.29 or later (instances use native sidecar containers) and a CNI that enforces
  NetworkPolicy.

### Without a sandbox

Setting `containers.runtimeClassName: ""` runs instances with the cluster's default runtime. They
then share their node's kernel: a container escape reaches the node. Do this only when every
tenant is trusted, for example a deployment that runs only your own code. Starts take about a
second instead of several, and no node needs KVM. The controller logs a warning at startup.

## Helm values

```yaml
containers:
  enabled: true
  runtimeClassName: kata-qemu
  podTemplate:
    nodeSelector: { katacontainers.io/kata-runtime: "true" }
  accountInstances: 20      # per account, at once
  totalInstances: 0         # installation-wide; 0 = no limit
  maxInstance: { vcpu: 4, memoryMiB: 12288, diskMB: 20000 }
  dns: [1.1.1.1, 8.8.8.8]   # instances' resolvers
  registry:
    host: registry.example.com
    tokenRealm: https://api.example.com/registry/token
    bundled: true
    storage: 20Gi
```

Tenants then deploy with:

```sh
CLOUDFLARE_API_BASE_URL=https://api.example.com/client/v4 \
CLOUDFLARE_CONTAINER_REGISTRY=registry.example.com wrangler deploy
```

Images live at `<registry host>/<account id>/<name>`. An account's credentials cover its own
repositories only, so one tenant cannot pull or overwrite another's images.

### Your own registry

Set `containers.registry.bundled: false` and configure your registry's token authentication:

| Setting (distribution's `auth.token`) | Value |
| --- | --- |
| `realm` | `containers.registry.tokenRealm` |
| `service` | `containers.registry.tokenService` (`selvedge-registry`) |
| `issuer` | `containers.registry.tokenIssuer` (`selvedge`) |
| `rootcertbundle` | `tls.crt` from the Secret `<release>-registry-token` |

To bring your own signing key, create a `kubernetes.io/tls` Secret with an ECDSA P-256 key and its
certificate and name it in `containers.registry.tokenSecret`.

## Single node

`selvedge all` runs instances on the local Docker daemon:

```sh
selvedge all --containers-docker-socket /var/run/docker.sock \
  --registry-host localhost:5000 \
  --registry-token-key token.key --registry-token-cert token.crt
```

There is no sandbox: use it for development and trusted code. `test/e2e/containers.sh` shows a
complete setup, registry included.

## Network

With `networkPolicy.enabled` the chart adds:

- Instances accept connections from runners only, on the sidecar's port. They may connect to
  runners' egress port (8791) and to the resolvers in `containers.dns` (port 53), and to nothing
  else: not each other, not the cluster's services, not the internet directly.
- The controller accepts connections from runners only.

The NetworkPolicy cannot tell one account's runner from another's; the egress relay can. It
identifies the instance by the connection's source address and hands it only to that instance's
own Durable Object.

## Operating

- **Limits.** `max_instances` per application, `containers.accountInstances` per account and
  `containers.totalInstances` per installation are checked under one lock per account, so
  concurrent starts cannot exceed them. A refused start fails in the Worker; the runner's log
  names the limit.
- **Cleanup.** The agent removes an account process's instances when the process ends. The
  controller also removes, every 30 seconds, instances whose runner pod is gone, instances that
  finished and were not collected, and instances that never started.
- **Metrics.** See [metrics.md](metrics.md): instances by state, starts by result, stops by
  reason, start duration, and Docker API calls the shim does not implement. That last one is the
  canary for a workerd upgrade that changes how workerd drives containers; the end-to-end tests in
  CI, which the workerd update workflow runs, cover the same.
- **Images.** Deleting an application deletes its repository's manifests (unless another of the
  account's applications uses the same repository), so its tags are gone at once. Layers stay
  on disk until the registry's garbage collection runs (`registry garbage-collect
  --delete-untagged`); schedule it. With your own registry, allow deletes and set
  `containers.registry.url` if the API cannot reach it at `https://<host>`.
