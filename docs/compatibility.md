# Compatibility

Selvedge aims to run Workers written for Cloudflare unchanged, and to accept the requests that
`wrangler` and the official SDKs send. This page lists what is supported and where behavior differs.
If you find a difference that is not listed, please open an issue.

## Runtime

Workers run on the official workerd release pinned in the image (currently `1.20261109.1`). A
compatibility date later than that release supports is lowered to the newest one it supports.

| Feature | Status | Notes |
| --- | --- | --- |
| ES module Workers, multiple modules | Supported | Module types: ES, CommonJS, text, data, WebAssembly, JSON, Python |
| Service-worker syntax | Not supported | Rejected on upload |
| Compatibility dates and flags | Supported | Passed to workerd as given, except flags that are experimental in workerd (`unsafe_module`, `rtti_api`, ...), which uploads may not use; Cloudflare does not offer them either |
| Outbound `fetch()` and `connect()` | Supported | Public addresses only; private, loopback and link-local addresses are refused |
| `request.cf` | Partial | `httpProtocol`; `colo` if the operator names the site (`--colo`); and fields the load balancer's headers carry, mapped with `--cf-header field=header` (Helm `gateway.cfHeaders`, needs `trustForwarded`): `country`, `city`, `continent`, `region`, `regionCode`, `postalCode`, `metroCode`, `timezone`, `latitude`, `longitude`, `asn`, `asOrganization`, `tlsVersion`, `tlsCipher`, `clientTcpRtt`, with `isEUCountry` derived from `country`. Selvedge has no geo database of its own. Clients cannot set it |
| Per-request CPU time limit (error 1102) | Different | Limits apply to the account's process (cgroup). A request stuck in a loop blocks every Worker of the account until the agent's health probe has failed for `--hang-timeout` (30 s); the agent then kills the process, which also fails the account's other in-flight requests |

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
| Workflows (`workflow`) | Supported | See below |
| Containers (`containers` in wrangler's configuration) | Supported when the operator enables them | See below |
| Vectorize, AI, Hyperdrive, Analytics Engine, and others | Not yet | An upload using one is rejected |

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

- Lifecycle rules can delete objects and abort uploads, but storage class transitions are accepted
  and ignored (every object is `Standard`).
- SSE-C, storage classes other than `Standard`, and jurisdictions are accepted and ignored.
- Parts of a multipart upload other than the last must be at least 5 MiB; R2's rule that they all be
  the same size is not enforced.

### Lifecycle rules

`wrangler r2 bucket lifecycle add|list|remove|set` work. A rule deletes objects under its prefix
once they are older than an age (`--expire-days`) or on a date (`--expire-date`), and aborts
multipart uploads left incomplete for an age (`--abort-multipart-days`). A bucket without rules has
Cloudflare's default one, which aborts uploads after 7 days. The data service applies the rules
every minute; deletions send `LifecycleDeletion` event notifications.

### Event notifications

`wrangler r2 bucket notification create|list|delete` work: a rule sends a message to a queue in the
account when an object whose key has the rule's prefix and suffix is created (`PutObject`,
`CopyObject`, `CompleteMultipartUpload`) or deleted (`DeleteObject`, `LifecycleDeletion`). Messages have Cloudflare's
shape (`account`, `action`, `bucket`, `object` with `key`, `size` and `eTag`, `eventTime`, and
`copySource` for copies), and changes from the binding, the S3 API and `wrangler r2` all send them.

Differences:

- Messages are enqueued in the same transaction as the change, so every stored change sends its
  messages and a change that fails sends none; delivery is then at least once, as for any queue.
- Deleting several objects at once sends one message per object that existed.

### Public buckets

`wrangler r2 bucket dev-url enable|disable|get` and `wrangler r2 bucket domain
add|list|get|update|remove` work. A bucket's public URL is `pub-<id>.<zone>`, where the operator
sets the zone (`SELVEDGE_R2_DEV_ZONE`; in Helm, `data.r2S3.devZone`) in place of `r2.dev`. A custom
domain must lie in a zone the operator assigned to the account, and `--zone-id` names that zone.
Both serve GET and HEAD of objects without credentials, with ranges and conditional requests, on
the data service's S3 listener: point `*.<zone>` and the custom domains at it.

Differences:

- The listener does not terminate TLS; put the load balancer or ingress that serves the hostnames
  in front of it. `minTLS` is stored for that front end and not enforced by Selvedge.
- There is no rate limiting on the public URL, and no caching in front of it.
- Requests for a hostname without an object key, and requests other than GET and HEAD, are
  refused; there is no directory listing.

### The S3-compatible API

The data service serves R2's S3 API (`--r2-s3-listen`; in Helm, the `<release>-r2-s3` Service).
Credentials work as on Cloudflare: the access key id is an account API token's id, and the secret
access key is the SHA-256 (hex) of the token's value. Creating a token returns both. Use region
`auto`. Path-style addressing (`forcePathStyle: true`, or rclone's `force_path_style`) always works;
virtual-hosted addressing (`<bucket>.<domain>`, the SDKs' default) works once the operator sets the
endpoint's domain (`SELVEDGE_R2_S3_DOMAIN`; Helm `data.r2S3.domain`) and points `*.<domain>` at it.

Supported: ListBuckets, CreateBucket, HeadBucket, DeleteBucket and GetBucketLocation; PutObject,
GetObject (ranges, `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`),
HeadObject, DeleteObject, DeleteObjects and CopyObject; conditional writes (`If-Match`,
`If-None-Match: *`); ListObjects and ListObjectsV2; multipart uploads (create, upload part,
complete, abort, ListParts). Requests are signed with SigV4 in headers or in presigned URLs (at
most 7 days). `aws-chunked` bodies, which current AWS SDKs send, are accepted. Objects are shared
with the binding and `wrangler r2`.

Differences:

- The signatures of individual `aws-chunked` chunks and trailing checksums are not checked; the
  request's signature and the object's MD5 are.
- Object tagging, ACLs, versioning, lifecycle rules, CORS and bucket policies are not available.
  ListMultipartUploads answers an empty list.
- The secret access key is the hash the API stores to check the token, so whoever can read the
  database can use the S3 API as that token. Cloudflare's model has the same property.

## D1

The binding is workerd's own D1 client (`cloudflare-internal:d1-api`), backed by a Selvedge system
Worker in which each database is a Durable Object with SQLite storage. Databases therefore have one
writer, durable commits and failover exactly like Durable Objects.

Supported: `prepare`/`bind` (numbers, strings, null, booleans, blobs), `first`, `all`, `run`, `raw`
(with `columnNames`), `batch` (one transaction: a failing statement rolls the whole batch back) and
`exec`. The REST API serves `wrangler d1 create`, `list`, `info`, `delete`, `execute --remote
--command`, `execute --remote --file`, `export --remote` and `migrations apply --remote`.

`d1 export` writes the schema and rows as SQL in SQLite's dump format (with `--no-schema`,
`--no-data` and `--table`), and `execute --file` runs a file in one transaction, so a failing
statement leaves the database as it was. Files move through URLs on the API itself that carry a
random token and expire after an hour.

Differences:

- SQL runs on Durable Object SQLite, so the same statements are refused as in a Durable Object
  (for example `BEGIN`/`COMMIT` and most `PRAGMA`s; D1 refuses these too).
- Not available yet: read replication and sessions (`withSession` works but always reads from the
  primary) and Time Travel.
- Imports and exports are held in memory by the database's Durable Object; an import file may be at
  most 64 MB (the API's upload limit).
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
  another runner takes the object over. The lease is fixed in workerd. A clean shutdown, such as
  a rolling upgrade or the agent's restart of a process over its memory limit, hands objects over
  without this wait: workerd lets requests in flight on each object finish (up to 4 seconds), then
  releases it to the next request anywhere, and the agent gives it workerd's full stop timeout
  (8 seconds, plus 2) before killing it. A kernel OOM kill (cgroup mode), a crash, or the agent
  killing a hung process still waits for the lease.
- Storage writes wait for the object store, so write latency depends on it (single-digit
  milliseconds for an in-cluster store).
- Requires an S3-compatible store that supports conditional writes (`If-None-Match: *` and
  `If-Match`). Tested: SeaweedFS 4.50. workerd refuses to start against a store without them.

## Containers

A Durable Object class can have a container, as on Cloudflare: `wrangler deploy` builds the image,
pushes it and creates the application, and the object controls its instance with `ctx.container`
(or the `@cloudflare/containers` library). Set `CLOUDFLARE_CONTAINER_REGISTRY` to the registry the
operator names. Each instance runs in its own pod, in a Kata VM unless the operator turned the
sandbox off. See [containers.md](containers.md) for running them.

| Feature | Status | Notes |
| --- | --- | --- |
| `start()` with `env`, `entrypoint`, `enableInternet` | Supported | `start()` may not name another image: an object runs its application's image only |
| `getTcpPort(port).fetch()` and `.connect()` | Supported | |
| `running`, `monitor()` with the exit code, `destroy()`, `setInactivityTimeout()` | Supported | |
| `signal()` | Partial | SIGTERM, SIGINT, SIGQUIT and SIGKILL stop the instance; other signals fail |
| `interceptOutboundHttp()`, `interceptOutboundHttps()` | Supported | For HTTPS the container must trust the sidecar's CA, installed at `/etc/cloudflare/certs/cloudflare-containers-ca.crt` |
| `exec()`: output, exit code, stdin, `env`, `cwd`, `pty` and `resize()`, `kill()` | Supported | `user` works with the Docker backend only; on Kubernetes an exec that names a user fails |
| `snapshotContainer()`, `snapshotDirectory()` | Not supported | Fail with a message naming the operation |
| `max_instances`, `instance_type` (named or `vcpu`/`memory_mib`/`disk.size_mb`) | Supported | Named types use Cloudflare's sizes; the operator sets the largest allowed |
| `wrangler deploy` of applications, and the applications API (list, get, modify, delete) | Supported | A rollout applies at once; there are no gradual or regional rollouts. A deploy with nothing changed creates no rollout |
| Locations, regions, SSH, GPUs, persistent disks, Cloudchamber deployments | Not supported | |

Differences:

- **Cold starts take seconds**: scheduling the pod, pulling the image the first time on a node, and
  booting its VM. On GitHub's runners (Kata under nested virtualization), a start took 8–9
  seconds with the image already on the node and 10–12 with a pull of a small image; nodes with
  direct KVM should be faster. Without the sandbox (Docker, one node) it is 1–2 seconds.
- The disk is ephemeral, as on Cloudflare: when an object moves to another runner (a deploy, a
  runner restart, a failover), its container starts afresh there.
- Containers reach only what workerd lets them: by default nothing, with `enableInternet` public
  addresses only. DNS goes to the operator's public resolvers even without `enableInternet`.
- An application's instances count against an account limit and an installation limit set by the
  operator, besides `max_instances`. A refused start fails like any other; the runner's log says why.

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

HTTP pull consumers work as on Cloudflare: `wrangler queues consumer http add|remove` (with
`--batch-size`, `--message-retries`, `--visibility-timeout-secs`, `--retry-delay-secs`,
`--dead-letter-queue`), then `POST .../queues/:id/messages/pull` leases a batch and
`POST .../messages/ack` acks or retries it by `lease_id`. A lease past its visibility timeout no
longer settles; the message is pulled again, and once out of retries it moves to the dead-letter
queue or is dropped. Producers can push over HTTP too: `POST .../messages` and `.../messages/batch`
(`json` or `text`, with `delay_seconds`). A queue has one consumer, a Worker or HTTP pull.

A Worker consumer gets a batch once `max_batch_size` messages are ready or the oldest has waited
`max_batch_timeout`. `wrangler queues pause-delivery|resume-delivery` stop and restart delivery to
either kind of consumer while producers keep sending; `wrangler queues update` sets a delivery delay
for messages sent without their own and the retention period (messages older than it are dropped);
`wrangler queues purge` deletes every message.

Differences:

- The `v8` content type (structured clone of arbitrary values) is refused by `send`. Decoding it for
  the consumer would need workerd's experimental `service_binding_extra_handlers` flag, which
  Selvedge does not enable for tenants. Use `json` (the default), `text` or `bytes`.
- `queue.metrics()` details beyond the backlog are not available yet.

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
- A file is deleted a day after the last version that uses it is deleted (versions are kept for
  rollbacks, so deleting a script is what frees its files).
- `assets.base_path` is ignored. Uploads use wrangler's bulk mode (several files per request).

## Workflows

Instances run on Cloudflare's own Workflows engine (from workers-sdk, run unmodified): one Durable
Object per instance, so they get cluster mode's single owner, durability and failover. A runner
crash pauses an instance for at most the Durable Object failover time, and it then carries on
where it was. The binding is workerd's own (`cloudflare-internal:workflows-api`): `create`,
`createBatch`, `get`, and on an instance `status`, `pause`, `resume`, `terminate`, `restart` and
`sendEvent`. Steps support `step.do` with retries and timeouts, `step.sleep`, `step.sleepUntil` and
`step.waitForEvent`.

`wrangler deploy` registers workflows. `wrangler workflows list`, `trigger`, `instances list`,
`describe` (with steps and attempts), `send-event`, `pause`, `resume`, `terminate`, `restart` and
`delete` work.

Differences:

- `schedules`, `concurrency` and `default_retention` in a workflow's config are accepted and
  ignored, and instances are kept until deleted.
- Versions are not tracked: `versionId` is empty, and an instance resumes on whatever code is
  deployed when it next runs.
- `instances list` shows instances newest first and does not filter by status or date.

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
- `request.cf` in events holds `httpProtocol` and `clientIp`. `--version-id` is accepted and does not filter.
- The WebSocket URL points at the API host the request came to (`Host`, or `X-Forwarded-Host` and
  `X-Forwarded-Proto` from a proxy), at `/tail/v1/<token>`. A proxy in front of the API must pass
  WebSocket upgrades on that path.

## Deployments and the API

`wrangler deploy`, `wrangler versions upload`, `wrangler versions deploy` (one version at 100%),
`wrangler rollback`, `wrangler delete`, `wrangler secret put/list/delete` and `wrangler kv` work
against `/client/v4`. Set `CLOUDFLARE_API_BASE_URL` to Selvedge's API.

- A deploy reaches the runners within about 2 seconds: agents follow a change feed every second and
  start the new version before retiring the old one.
- Gradual deployments (`wrangler versions deploy a@90% b@10%`) split HTTP requests between two
  versions by percentage. `Cloudflare-Workers-Version-Key` keeps a client on one version and
  `Cloudflare-Workers-Version-Overrides` picks one, as on Cloudflare. Service bindings, Cron Triggers
  and queue batches go to the version with the larger share. Workers that define Durable Objects or
  have static assets cannot be split.
- Authentication is by API token (`Authorization: Bearer`). Global API keys are not supported.
- `inherit` bindings, which wrangler sends for resources the deployed version already binds, keep
  the deployed version's binding of that name; an upload naming one it does not have is refused.
- Hostnames: `<script>.<account subdomain>.<zone>` for scripts with workers.dev enabled, and custom
  domains (`/workers/domains`), and zone routes.
- Zone routes: an operator assigns a zone to an account with an admin token (`POST /zones` with
  `{"name", "account": {"id"}}`), because Selvedge cannot check who owns a domain. The account's
  `routes` (`example.com/api/*`, `*.example.com/*`) are then set by `wrangler deploy` or the zone
  routes API. A host may start with `*` and a path may end with `*`, and the most specific pattern
  wins. Custom domains and workers.dev hostnames take precedence over routes, as on Cloudflare. A
  route without a script keeps its requests from Workers.
