# Selvedge

**A highly available, Workers-compatible platform for Kubernetes, built on the official workerd runtime.**

Selvedge runs Cloudflare Workers-style applications on your own Kubernetes cluster. You deploy with the
tools you already use (`wrangler deploy` works against it), and it keeps serving through node and
availability-zone failures.

> Selvedge is an independent open-source project. It is not affiliated with or endorsed by Cloudflare.
> "Cloudflare" and "Workers" are trademarks of Cloudflare, Inc.; they are used here only to describe
> API compatibility.

Status: **early development** ([latest release](https://github.com/LaciaProject/selvedge/releases/latest)). The API and
storage formats can still change.

## Why

[workerd](https://github.com/cloudflare/workerd) is the open-source runtime behind Cloudflare Workers.
It runs isolates, but it is not a platform: it has no multi-tenant routing, deployment API, storage
services or failover. Selvedge is that layer, for Kubernetes:

- **Official workerd, unmodified.** No fork to maintain. Selvedge only uses workerd's public
  configuration, so upgrading workerd means bumping one version.
- **No single points of failure.** Every Selvedge component is a stateless Deployment. State lives in
  PostgreSQL and an S3-compatible object store, which you can run highly available.
- **Durable Objects that fail over.** Selvedge uses workerd's cluster mode: each object has exactly one
  owner, its state is committed to the object store before a response leaves, and when a node dies
  another takes its objects over within about 20 seconds.
- **The data plane outlives the control plane.** Gateways and runners cache routes, code and
  configuration. If the API or the database is down, deployed Workers keep serving.
- **Multi-tenant by default.** Each account runs in its own workerd process, with no access to platform
  credentials, private networks or other accounts.

## Architecture

```
client ─▶ LB ─▶ gateway (×3) ── host → account/script, rendezvous-hashed to runners
                    │
                    ▼
               runner (×N): agent + one workerd process per account
                    │   ├─ KV, R2 calls ─────▶ data (×3) ─▶ PostgreSQL + object store
                    │   └─ Durable Objects ──▶ S3-compatible bucket (workerd cluster mode)
                    ▼
               api (×2): /client/v4, used by wrangler
```

The design and its trade-offs are in [RFC 0001](docs/rfcs/0001-architecture.md).

## What works

| Supported | Not yet |
| --- | --- |
| ES module Workers, multiple modules, `nodejs_compat` and other compatibility flags | Service-worker syntax |
| `wrangler deploy`, `versions upload/deploy` (including gradual, two-version deployments), `rollback`, `delete`, `secret put/delete` | |
| KV (binding and REST API, `wrangler kv`) | |
| Workflows (binding, `wrangler workflows` trigger/list/describe/events/pause/resume/terminate/delete) | Workflow schedules, per-instance retention |
| Durable Objects: fetch, WebSockets, JS RPC, SQLite and KV storage, alarms | Workers AI, Hyperdrive, Analytics Engine |
| Static assets (`assets` in wrangler config, `_headers`, `_redirects`, `run_worker_first`, the `ASSETS` binding; unused files are collected) | |
| R2 (binding, multipart, conditional requests; `wrangler r2`; the S3-compatible API with presigned URLs; public bucket URLs and custom domains; event notifications to queues; lifecycle rules) | |
| D1 (binding, batches, `wrangler d1 create/execute/migrations/export`, `execute --file`) | D1 read replicas, Time Travel |
| Cron Triggers (`triggers.crons`, including `L`, `W` and `#`) | |
| Queues (producers, Worker and HTTP pull consumers, retries, dead-letter queues, pushing over HTTP; `wrangler queues`) | `v8` message format |
| Service bindings, environment variables, secrets, JSON bindings | Tail Workers, Logpush |
| `wrangler tail` (logs, exceptions, filters) | |
| workers.dev-style hostnames, custom domains and zone routes (`routes` in wrangler config) | |

Uploads that use an unsupported binding are rejected rather than deployed without it. See
[docs/compatibility.md](docs/compatibility.md) for details and known differences, and
[docs/status.md](docs/status.md) for the project's progress and limitations.

## Quick start

### On Kubernetes

```sh
helm install slv deploy/helm/selvedge --namespace selvedge --create-namespace
# For production: --set profile=ha with an HA PostgreSQL and S3 store; see values.yaml.
```

The chart's notes print how to get the admin token, create an account and deploy.

### On one machine

```sh
cargo build --release
npm install -g workerd@1.20261113.1 wrangler   # or point SELVEDGE_WORKERD at a workerd binary

export SELVEDGE_DATABASE_URL=postgres://user:pass@localhost/selvedge
export SELVEDGE_BLOB_URL=file://$HOME/.local/share/selvedge/blobs
export SELVEDGE_MASTER_KEY=$(openssl rand -hex 32)
export SELVEDGE_INTERNAL_TOKEN=$(openssl rand -hex 16)
export SELVEDGE_ADMIN_TOKEN=$(openssl rand -hex 16)
export SELVEDGE_WORKERS_DEV_ZONE=workers.localhost
export SELVEDGE_STATE_DIR=$HOME/.local/share/selvedge
export SELVEDGE_RUN_DIR=$XDG_RUNTIME_DIR/selvedge   # keep short: Unix socket paths are limited
./target/release/selvedge all                      # API :8787, gateway :8080
```

Then create an account and deploy:

```sh
API=http://localhost:8787/client/v4
curl -s -X POST $API/accounts -H "Authorization: Bearer $SELVEDGE_ADMIN_TOKEN" -d '{"name":"me"}'
curl -s -X POST $API/accounts/$ACCOUNT/selvedge/tokens -H "Authorization: Bearer $SELVEDGE_ADMIN_TOKEN" -d '{"name":"cli"}'

CLOUDFLARE_API_BASE_URL=$API CLOUDFLARE_API_TOKEN=$TOKEN CLOUDFLARE_ACCOUNT_ID=$ACCOUNT wrangler deploy
curl -H "Host: my-worker.$SUBDOMAIN.workers.localhost" http://localhost:8080/
```

## Development

```sh
cargo test                                  # unit tests
SELVEDGE_WORKERD=/path/to/workerd cargo test  # plus tests against the real workerd
test/e2e/single-node.sh                     # wrangler against `selvedge all` (needs PostgreSQL)
test/e2e/cluster.sh                         # Durable Object failover on two runners (needs S3)
test/bench/bench.sh                         # request-path throughput and latency, with mTLS
```

The multi-node Kubernetes tests (`test/k8s/`) run on GitHub Actions: push a branch named `ci/...`.
See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache-2.0](LICENSE).
