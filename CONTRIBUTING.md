# Contributing to Selvedge

Thanks for your interest. Selvedge is early, so the most useful contributions right now are bug
reports with reproductions, compatibility findings ("this Worker behaves differently on Selvedge"),
and focused pull requests.

## Ground rules

- **No workerd fork.** Selvedge uses the official workerd release only. If something needs a workerd
  change, propose it upstream; Selvedge adopts it when it ships.
- **Big changes start as an RFC.** Anything that changes the architecture, a storage format or a
  public API gets a short design document in `docs/rfcs/` first, discussed in a pull request.
- **Compatibility is explicit.** When behavior differs from Cloudflare's, document it in
  `docs/compatibility.md`. An unsupported feature is rejected, never silently ignored.
- **Migrations are append-only.** Never edit a released migration in
  `crates/selvedge-store/migrations/`; add a new one.

## Developer Certificate of Origin

Every commit must be signed off, certifying the [Developer Certificate of Origin](https://developercertificate.org/):

```sh
git commit -s
```

This adds a `Signed-off-by: Your Name <you@example.com>` line. There is no CLA.

## Workflow

1. `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must pass.
2. Set `SELVEDGE_WORKERD` to a workerd binary to include the tests that run it
   (`npm install workerd@<version>` in `test/e2e` provides one).
3. End-to-end tests need PostgreSQL (and, for `cluster.sh`, an S3 store with conditional writes such
   as SeaweedFS). CI runs them on every pull request.
4. Large tests (multi-node Kubernetes, chaos, upgrades) run on GitHub Actions only. Push a branch
   named `ci/<topic>` or run the `kubernetes` workflow by hand.

## Releases

Set `version` and `appVersion` in `deploy/helm/selvedge/Chart.yaml`, merge, then push a tag
`v<version>` on `main`. The `release` workflow checks that the tag matches the chart, pushes the
image (`ghcr.io/laciaproject/selvedge:<version>`, linux/amd64 and linux/arm64) and the chart
(`oci://ghcr.io/laciaproject/charts/selvedge`), and creates the GitHub release.

## Layout

| Path | What |
| --- | --- |
| `crates/selvedge-core` | Identifiers, the worker and binding model |
| `crates/selvedge-store` | PostgreSQL schema and queries, blob store, secret encryption |
| `crates/selvedge-workerd` | workerd config rendering and process supervision |
| `crates/selvedge-server` | The roles: api, data, agent, gateway |
| `crates/selvedge` | The `selvedge` binary |
| `deploy/helm/selvedge` | Helm chart |
| `test/e2e`, `test/k8s` | End-to-end and Kubernetes tests |
| `docs/rfcs` | Design documents |

## Security issues

Do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).
