# Web dashboard

Selvedge has a web dashboard in the style of Cloudflare's, built with Cloudflare's open-source React
components ([Kumo](https://github.com/cloudflare/kumo)). It shows and manages what an account runs:

- Workers: versions, deployments, traffic splits, bindings, secrets, routes, custom domains, Cron
  Triggers and live logs;
- storage: KV keys and values, a D1 SQL console with import and export, and an R2 object browser
  with public access, custom domains, lifecycle rules and event notifications;
- Queues, Workflows, Durable Objects and Containers;
- an account's members and API tokens.

Platform admins also manage accounts, users and zones. The interface is in English and Simplified
Chinese. Its design is [RFC 0003](rfcs/0003-dashboard.md).

The dashboard has its own domain, such as `https://dash.example.com`. The API process serves it on
a listener of its own. That listener takes only the dashboard's session cookie, and the API's
listener takes only Bearer tokens, as before, so wrangler is unaffected. Both run the same
`/client/v4` handlers, so permissions are checked in one place.

## Turning it on

With the Helm chart, set the dashboard's URL, route that domain to the `<release>-dash` Service
(port 8785), and terminate TLS in front of it:

```yaml
dash:
  url: https://dash.example.com
```

The cookie is `__Host-` prefixed and `Secure` whenever the URL is `https://`; only a local `http://`
URL (`localhost`) goes without. The chart's other `dash.*` values are below; the image carries the
built files at `/usr/share/selvedge/dash`.

Without the chart, `selvedge api` (or `selvedge all`) takes these flags:

| Flag | Environment | Default | |
| --- | --- | --- | --- |
| `--dash-listen` | `SELVEDGE_DASH_LISTEN` | off | Where the dashboard listens |
| `--dash-url` | `SELVEDGE_DASH_URL` | | Its public URL (required with `--dash-listen`) |
| `--dash-dir` | `SELVEDGE_DASH_DIR` | | The built files (`dash/dist`) |
| `--dash-token-login` | `SELVEDGE_DASH_TOKEN_LOGIN` | `true` | Allow logging in with an API token |
| `--dash-session-idle` | `SELVEDGE_DASH_SESSION_IDLE` | 28800 | Seconds a session may sit idle |
| `--dash-session-max` | `SELVEDGE_DASH_SESSION_MAX` | 86400 | Seconds a session lasts at most |

## Logging in

**With an API token.** Paste an admin token or an account token. The server checks it and starts
a session; the token itself stays out of the browser. This is for small installations and for when
the provider is down. Turn it off with `dash.tokenLogin: false` once single sign-on works. An
account token's session cannot manage tokens.

**With OpenID Connect.** Any standard provider works: Keycloak, Dex, Authentik, Okta, Microsoft
Entra ID, Google and others. Selvedge uses the authorization code flow with PKCE, as a confidential
client. It reads everything else from the issuer's discovery document, and checks ID tokens' issuer,
audience, `azp`, expiry, issue time, nonce and signature (RS, PS, ES and EdDSA; never `none` or
HMAC).

1. Register a client with the provider. Its redirect URI is `<dash.url>/auth/callback`.
2. Put its secret in a Secret with the key `client-secret`.
3. Configure the chart:

```yaml
dash:
  url: https://dash.example.com
  oidc:
    issuer: https://id.example.com/realms/main   # exactly as the provider names itself
    clientId: selvedge
    existingSecret: selvedge-oidc                 # key: client-secret
    name: Example SSO                             # on the login button
    scopes: [openid, email, profile, groups]      # whatever makes the provider send groups
    groupsClaim: groups                           # or a dotted path: realm_access.roles
    adminGroups: [platform-admins]
    adminEmails: [ops@example.com]
```

The flags are `--oidc-issuer`, `--oidc-client-id`, `--oidc-client-secret`, `--oidc-name`,
`--oidc-scopes`, `--oidc-groups-claim`, `--oidc-admin-groups` and `--oidc-admin-emails`, or the
same names as `SELVEDGE_OIDC_*` variables.

When the ID token lacks the email or the groups, Selvedge asks the userinfo endpoint. A provider
that offers RP-initiated logout (`end_session_endpoint`) is logged out of too; register
`<dash.url>/login` as its post-logout redirect URI.

## Who may do what

People are known by the provider's `(issuer, subject)`, not by email, which can change.

- **Platform admins**: people in `adminGroups`, or with a verified email in `adminEmails`, as of
  their last login. Admin tokens count as admins too. They see every account, and manage accounts,
  members, users and zones.
- **Members** of an account can do everything in it, as an account token can, and make and revoke
  its tokens. Someone becomes a member in one of two ways:
  - Invitation by email: an admin invites an email address, and whoever first logs in with it,
    verified by the provider, becomes the member. From then on the membership follows that person,
    not the email.
  - The provider's group: an admin sets the account's group, and everyone in it is a member. Groups
    are read at each login, so leaving the group takes effect at the next one.

Removing a member takes effect on their next request. Removing a user ends their memberships and
sessions. Tokens a person made keep working until someone revokes them; the token list marks those
whose creator is no longer a member.

## Security

- **Sessions** live in PostgreSQL. The browser holds a random id in an `HttpOnly`, `SameSite=Lax`
  cookie, never a token. Logging out, revoking the token a session started from, or removing the
  user ends it.
- **Cross-site requests:** every API call from the dashboard carries `X-Selvedge-Dash: 1`, which a
  form or tag on another site cannot set. Calls that change something must also come from the
  dashboard's `Origin`. The live-logs WebSocket is checked by its `Origin`.
- **Pages** are served with a CSP that allows only the dashboard's own scripts and connections. No
  CDN is used. The pages cannot be framed. User content (KV values, objects, SQL results, logs) is
  shown as text.
- **Audit:** every change made through the dashboard is logged as `dashboard change`, with the
  method, the path, the status, and the user's id and email.

## API additions

The dashboard uses the `/client/v4` API, plus these Selvedge additions. Scripts with an admin token
can call them too.

| Endpoint | |
| --- | --- |
| `GET/POST /accounts/{id}/selvedge/members`, `DELETE …/members/{member}` | List members; invite (`{"email"}`) or add a known user (`{"user_id"}`) (admins) |
| `GET/PUT /accounts/{id}/selvedge/oidc-group` | The account's provider group (`PUT`: admins) |
| `GET /accounts/{id}/selvedge/tokens`, `DELETE …/tokens/{token}` | List and revoke account tokens (not with an account token) |
| `GET /selvedge/users`, `DELETE /selvedge/users/{user}` | People who have logged in (admins) |
| `DELETE /accounts/{id}` | Delete an account and everything recorded for it (admins) |
| `GET /accounts/{id}/r2/buckets/{bucket}/objects` | List objects, with `prefix`, `delimiter`, `cursor` and `per_page`, as Cloudflare does |

`GET /accounts/{id}/queues/{queue}` also returns `message_backlog_count`.

Deleting an account removes its database records and R2 data. Durable Object storage (D1's too)
stays in the runners' cluster bucket under the account's prefix, which the API cannot reach; the
operator removes it.

## Grafana

Operators' metrics are a separate thing: see the Grafana dashboard in [metrics.md](metrics.md).

## Development

```sh
cd dash
npm ci
SELVEDGE_DASH=http://localhost:8790 npm run dev  # proxies to a `selvedge all --dash-listen 127.0.0.1:8790`
npm run check     # types, lint, and the two languages' messages
npm test          # unit tests
npm run build && npm run size   # the first screen must stay under 300 KB compressed
```

Every word on screen comes from `src/i18n/en.json` and `src/i18n/zh-CN.json`. The check fails when
a key is missing from either file, and ESLint rejects literal text in JSX. Use Kumo's semantic color
tokens, never raw Tailwind colors or `dark:` variants.

`test/e2e/dash.sh` is the end-to-end test. It starts `selvedge all` and Dex, seeds an account, and
runs the Playwright suite (`dash/e2e`): the main flows in both languages, the security checks, and
single sign-on, with admins by group, members by invitation and by group, and removal.
