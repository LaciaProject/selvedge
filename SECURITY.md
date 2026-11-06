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

Known gaps, tracked for the next releases:

- Account processes in a runner pod share one Linux user and cgroup; per-account UIDs, cgroups and
  seccomp profiles are planned (RFC 0001, spike S6).
- Durable Object storage uses one set of object store credentials for every account; per-account,
  prefix-scoped credentials are planned.
- Platform components authenticate each other with a shared token; mTLS is planned.
