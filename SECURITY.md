# Security policy

Selvedge runs untrusted code from many tenants, so we treat isolation bugs as the most serious class
of vulnerability.

## Reporting

Report vulnerabilities privately through
[GitHub Security Advisories](https://github.com/LaciaProject/selvedge/security/advisories/new).
Please do not open a public issue.

We aim to acknowledge reports within 3 business days and to agree on a disclosure date with you.
Credit is given in the advisory unless you prefer otherwise.

In scope, most severe first:

1. Tenant code escaping its account: reading another account's data, secrets or code, or calling
   another account's Workers or Durable Objects.
2. Tenant code reaching platform internals: the internal token, database or object store
   credentials, the data service, private networks, or cloud metadata endpoints.
3. Bypassing `/client/v4` authentication or account scoping.
4. Denial of service that one tenant can cause for others.

workerd sandbox escapes should also be reported to Cloudflare
([workerd security policy](https://github.com/cloudflare/workerd/security/policy)); we will
coordinate.

## Supported versions

Until 1.0, only the latest release receives fixes.

## Following workerd

Selvedge pins an official workerd release. When workerd publishes a security fix, we release a
Selvedge version with the new workerd within **7 days**.

## Defense in depth

The isolation model is described in [RFC 0001, §8](docs/rfcs/0001-architecture.md). In short: one
workerd process per account, started with an empty environment; tenant outbound traffic limited to
public addresses by workerd and by a NetworkPolicy; no database or platform credentials in runner
processes; binding calls stamped with the account identity by the agent, outside tenant reach.

With `--account-uids` (on by default in the Helm chart, `runner.accountUsers`), each account's
workerd runs as its own Linux user with no capabilities. Its directories are 0700, and its binding
socket, which stamps the account's identity on KV, R2, D1 and queue calls, is 0600. So a
compromised process can neither read another account's files nor act as another account. The
agent then runs as root in the container with only CAP_SETUID, CAP_SETGID, CAP_CHOWN,
CAP_DAC_OVERRIDE, CAP_FOWNER and CAP_KILL, and refuses to run as root without `--account-uids`.
`runner.hostUsers: false` adds a user namespace, so root in the container is unprivileged on the
node. Runner pods use the `RuntimeDefault` seccomp profile.

Compatibility flags that workerd marks experimental are refused on upload and dropped when
rendering. Processes with Durable Objects run with `--experimental` (cluster mode), so otherwise a
tenant could enable them, and some, such as `unsafe_module` and
`allow_insecure_inefficient_logged_eval`, weaken the sandbox.

workerd processes never hold the object store's credentials. Durable Object storage goes through
an S3 proxy in the agent. Each account's process gets a key derived for its account alone, and the
proxy checks the SigV4 signature, allows only that account's prefix and the operations cluster mode
uses, then signs the request again with the real credentials. `request.cf` is set by the gateway,
never from the runner's local sockets.

Known gaps, tracked for the next releases:

- Account processes in a runner pod share one cgroup: one account can use the pod's whole CPU and
  memory limit. Per-account cgroups need a delegated cgroup tree, which Kubernetes does not give
  containers by default (RFC 0001, spike S6).
- Platform components authenticate each other with a shared token; mTLS is planned.
