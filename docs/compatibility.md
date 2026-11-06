# Compatibility

Selvedge aims to run Workers written for Cloudflare unchanged, and to accept the requests that
`wrangler` and the official SDKs send. This page lists what is supported and where behavior differs.
If you find a difference that is not listed, please open an issue.

## Runtime

Workers run on the official workerd release pinned in the image (currently `1.20261106.1`). A
compatibility date later than that release supports is lowered to the newest one it supports.

| Feature | Status | Notes |
| --- | --- | --- |
| ES module Workers, multiple modules | Supported | Module types: ES, CommonJS, text, data, WebAssembly, JSON, Python |
| Service-worker syntax | Not supported | Rejected on upload |
| Compatibility dates and flags | Supported | Passed to workerd as given |
| Outbound `fetch()` and `connect()` | Supported | Public addresses only; private, loopback and link-local addresses are refused |
| `request.cf` | Not supported | |
| Per-request CPU time limit (error 1102) | Different | Limits apply to the account's process (cgroup); a runaway request is stopped by restarting the process, which also fails the account's other in-flight requests |

## Bindings

| Binding | Status | Notes |
| --- | --- | --- |
| Environment variables (`plain_text`), `json` | Supported | |
| Secrets (`secret_text`, `wrangler secret`) | Supported | Encrypted at rest (AES-256-GCM) |
| Service bindings, including named entrypoints | Supported | Within one account. A binding to a script that is not deployed answers 503 |
| KV | Supported | See below |
| Durable Objects | Supported | See below |
| R2, D1, Queues, Workflows, Vectorize, AI, Hyperdrive, Analytics Engine, and others | Not yet | An upload using one is rejected |

## KV

All operations of the binding are supported: `get` (text, JSON, ArrayBuffer, stream), `get` of up to
100 keys, `getWithMetadata`, `put` with `expiration`, `expirationTtl` and `metadata`, `delete` and
`list` with `prefix`, `limit` and `cursor`. Limits match Cloudflare's: 512-byte keys, 25 MiB values,
1024-byte metadata, a 60-second minimum TTL.

Differences:

- Reads are strongly consistent: a write is visible to every reader at once. Cloudflare allows up to
  60 seconds of staleness, so code written for Cloudflare works unchanged.
- `cacheTtl` is accepted and ignored.

## Durable Objects

Durable Objects run on workerd's cluster mode. Each object has one owner process in the cluster, and
its storage commits are acknowledged only once the object store holds them.

| Feature | Status |
| --- | --- |
| `fetch`, JS RPC, WebSockets with hibernation | Supported |
| SQLite storage (`ctx.storage.sql`) and the KV storage API | Supported |
| Alarms | Supported |
| `idFromName`, `newUniqueId`, `idFromString` | Supported |
| Location hints, jurisdictions | Ignored: everything runs in one cluster |
| Migrations (`new_classes`, `renamed_classes`, `deleted_classes`) | Accepted; renames are not applied yet |

Differences:

- **Failover takes up to about 20 seconds.** When the process owning an object dies, calls to that
  object fail (as with Cloudflare's "Network connection lost") until the owner's lease expires and
  another runner takes the object over. A clean shutdown, such as a rolling upgrade, hands objects
  over without this wait.
- Storage writes wait for the object store, so write latency depends on it (single-digit
  milliseconds for an in-cluster store).
- Requires an S3-compatible store that supports conditional writes (`If-None-Match: *` and
  `If-Match`). Tested: SeaweedFS 4.50. workerd refuses to start against a store without them.

## Deployments and the API

`wrangler deploy`, `wrangler versions upload`, `wrangler versions deploy` (one version at 100%),
`wrangler rollback`, `wrangler delete`, `wrangler secret put/list/delete` and `wrangler kv` work
against `/client/v4`. Set `CLOUDFLARE_API_BASE_URL` to Selvedge's API.

- A deploy reaches the runners within about 2 seconds.
- Gradual deployments (two versions with percentages) are rejected.
- Authentication is by API token (`Authorization: Bearer`). Global API keys are not supported.
- Hostnames: `<script>.<account subdomain>.<zone>` for scripts with workers.dev enabled, and custom
  domains (`/workers/domains`). Zone routes are not supported yet.
