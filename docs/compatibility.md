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
| R2 | Supported | See below |
| D1 | Supported | See below |
| Queues, Workflows, Vectorize, AI, Hyperdrive, Analytics Engine, and others | Not yet | An upload using one is rejected |

## KV

All operations of the binding are supported: `get` (text, JSON, ArrayBuffer, stream), `get` of up to
100 keys, `getWithMetadata`, `put` with `expiration`, `expirationTtl` and `metadata`, `delete` and
`list` with `prefix`, `limit` and `cursor`. Limits match Cloudflare's: 512-byte keys, 25 MiB values,
1024-byte metadata, a 60-second minimum TTL.

Differences:

- Reads are strongly consistent: a write is visible to every reader at once. Cloudflare allows up to
  60 seconds of staleness, so code written for Cloudflare works unchanged.
- `cacheTtl` is accepted and ignored.

## R2

The binding supports `head`, `get` (with ranges, suffix ranges and `Range` headers), `put` (with HTTP
and custom metadata, MD5 and SHA checksums), `delete` (one or many keys), `list` (prefix, delimiter,
cursor, `startAfter`, `include`), conditional requests (`onlyIf` with ETags and upload times) and
multipart uploads. ETags are the MD5 of the object, or `<md5 of part MD5s>-<parts>` for multipart
uploads, as in R2. Object metadata is strongly consistent: it lives in PostgreSQL and every
conditional write is one transaction.

`wrangler r2 bucket create/list/delete` and `wrangler r2 object put/get/delete` work.

Differences:

- R2's S3-compatible endpoint, presigned URLs, public buckets and custom domains on buckets are not
  available yet.
- SSE-C, storage classes other than `Standard`, and jurisdictions are accepted and ignored.
- Parts of a multipart upload other than the last must be at least 5 MiB; R2's rule that they all be
  the same size is not enforced.

## D1

The binding is workerd's own D1 client (`cloudflare-internal:d1-api`), backed by a Selvedge system
Worker in which each database is a Durable Object with SQLite storage. Databases therefore have one
writer, durable commits and failover exactly like Durable Objects.

Supported: `prepare`/`bind` (numbers, strings, null, booleans, blobs), `first`, `all`, `run`, `raw`
(with `columnNames`), `batch` (one transaction: a failing statement rolls the whole batch back) and
`exec`. The REST API serves `wrangler d1 create`, `list`, `info`, `delete`, `execute --remote
--command` and `migrations apply --remote`.

Differences:

- SQL runs on Durable Object SQLite, so the same statements are refused as in a Durable Object
  (for example `BEGIN`/`COMMIT` and most `PRAGMA`s; D1 refuses these too).
- Not available yet: read replication and sessions (`withSession` works but always reads from the
  primary), Time Travel, `wrangler d1 export`, and `execute --file` for large imports.
- `meta.rows_read` and `meta.rows_written` count what SQLite reports, which can differ from D1's
  billing counters.

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

- A deploy reaches the runners within about 2 seconds: agents follow a change feed every second and
  start the new version before retiring the old one.
- Gradual deployments (two versions with percentages) are rejected.
- Authentication is by API token (`Authorization: Bearer`). Global API keys are not supported.
- Hostnames: `<script>.<account subdomain>.<zone>` for scripts with workers.dev enabled, and custom
  domains (`/workers/domains`). Zone routes are not supported yet.
