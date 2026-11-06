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
| Queues (producers) | Supported | See below |
| Static assets (`assets`) | Supported | See below |
| Workflows, Vectorize, AI, Hyperdrive, Analytics Engine, and others | Not yet | An upload using one is rejected |

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

## Cron Triggers

`triggers.crons` in a wrangler config, or `PUT /workers/scripts/:name/schedules`, sets up to five
triggers per script. Expressions have five fields in UTC and support Cloudflare's `L`, `W` and `#`.
Firings are claimed in PostgreSQL by whichever data service replica gets there first, so each runs
once.

Differences:

- `event.noRetry()` is accepted but has no effect: like Cloudflare, Selvedge does not retry a
  failed firing.
- A trigger that falls behind (for example while PostgreSQL is down) fires once when it can, not
  once per missed slot.
- `wrangler dev --test-scheduled` is a local feature and is unaffected.

## Queues

Producers use workerd's own `Queue` binding: `send` and `sendBatch`, with `delaySeconds` and the
`json` (default), `text` and `bytes` content types. Messages are rows in PostgreSQL. Worker
consumers (`queues.consumers` in a wrangler config, or `wrangler queues consumer add`) receive
batches with `ack()`, `retry({ delaySeconds })`, `ackAll()` and `retryAll()`, and Cloudflare's rules
for what a returning or throwing handler means. Messages that run out of retries move to the
dead-letter queue if one is set, else they are dropped. `wrangler queues create/list/delete` work.

Delivery is at least once: a batch is leased in the database while it is delivered, so data service
replicas never deliver a message twice at the same time, and a lease left by a crashed replica
expires and the batch is delivered again.

Differences:

- The `v8` content type (structured clone of arbitrary values) is refused by `send`. Decoding it for
  the consumer would need workerd's experimental `service_binding_extra_handlers` flag, which
  Selvedge does not enable for tenants. Use `json` (the default), `text` or `bytes`.
- Batches are delivered as soon as messages are ready; `max_batch_timeout` is stored but does not
  hold messages back to fill a batch.
- HTTP pull consumers, `queue.metrics()` details beyond the backlog, and pausing delivery are not
  available yet.

## Static Assets

`wrangler deploy` with an `assets` directory works, for a Worker with code and for a Worker that only
has assets. Assets are served by Cloudflare's own asset and router Workers (from workers-sdk, run
unmodified on workerd), so `html_handling`, `not_found_handling` (`single-page-application`,
`404-page`), `run_worker_first` (a boolean or a list of rules), `_headers`, `_redirects`, ETags and
conditional requests behave as on Cloudflare. Selvedge parses `_headers`, `_redirects` and
`run_worker_first` rules at deploy time, as Cloudflare's API does, and skips invalid lines the same
way. The `ASSETS` binding (`env.ASSETS.fetch`) reads the Worker's own assets.

Uploads skip files the account already has, so a redeploy sends only what changed. The limits match
Cloudflare's: 25 MiB per file, 100,000 files per version.

Asset bytes live in the blob store; each runner keeps the ones it has served on local disk (1 GiB by
default, `--asset-cache-mb`), so a runner keeps serving them while the data service is unavailable.

Differences:

- A service binding to a Worker that has code calls that code directly, which named entrypoints and
  RPC need. A service binding to a Worker with only assets reaches its assets.
- Uploaded files are not deleted when no version uses them any more.
- `assets.base_path` is ignored. Uploads use wrangler's bulk mode (several files per request).

## wrangler tail

`wrangler tail` works in both formats (`pretty` and `json`), with its filters: `--status`,
`--method`, `--header`, `--ip` (including `self`), `--search` and `--sampling-rate`. Events have
Cloudflare's shape: the request, the response status, the outcome, `console` logs and uncaught
exceptions with their stacks. They cover fetch requests and Durable Object calls. Cron Triggers and
queue batches appear as RPC events (`scheduled`, `queue`), because Selvedge delivers them over JS
RPC.

How it works: every Worker reports to a tail worker in its account's process. That worker sends
events on only while someone is tailing the script; otherwise it stays quiet. The API applies the
filters and passes events to the API replica that holds the WebSocket, through PostgreSQL
`LISTEN`/`NOTIFY`.

Differences:

- Delivery is best effort, as on Cloudflare: a client that falls behind loses events.
- An event larger than about 7.9 KB, PostgreSQL's limit on a notification, is shortened: log
  lines, exception messages and header values are cut, and `truncated` is set.
- `request.cf` holds only `clientIp`. `--version-id` is accepted and does not filter.
- The WebSocket URL points at the API host the request came to (`Host`, or `X-Forwarded-Host` and
  `X-Forwarded-Proto` from a proxy), at `/tail/v1/<token>`. A proxy in front of the API must pass
  WebSocket upgrades on that path.

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
