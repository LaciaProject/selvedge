//! Containers (RFC 0002): the `/containers` API `wrangler deploy` uses to
//! push an image and attach it to a Durable Object class, and the registry's
//! token service.
//!
//! An application is stored as the client sent it and answered the same way,
//! so `wrangler` sees no difference on the next deploy. Rollouts apply at
//! once: changing an application bumps the account's generation, runners
//! start a new process for the account, and its containers start again with
//! the new image as their Durable Objects are next used.

use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use selvedge_core::containers::{Resources, do_namespace_id, resources_of, split_image};
use selvedge_core::{AccountId, Binding};
use selvedge_store::{ContainerApp, ContainerAppFields};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, JsonBody, State, account_access};
use crate::error::{ApiError, ApiResult, ok};
use crate::registry::{Credentials, Permissions, TokenSigner, USERNAME};

/// What the API needs to serve containers.
pub struct ContainersConfig {
    /// The registry's host (and port), as image references name it. Images
    /// must live under `<host>/<account id>/`.
    pub registry_host: String,
    /// Where the API reaches the registry itself, to delete a deleted
    /// application's images (`https://<host>` unless set).
    pub registry_url: String,
    /// Signs registry tokens; without it, credentials cannot be used.
    pub signer: Option<TokenSigner>,
    pub credentials: Credentials,
    /// The most one instance may ask for.
    pub max_instance: Resources,
}

/// `max_instances` when an application does not say.
const DEFAULT_MAX_INSTANCES: i64 = 20;
/// The longest a credential may be asked for, in minutes.
const MAX_CREDENTIAL_MINUTES: u64 = 24 * 60;

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/containers";
    Router::new()
        .route(&format!("{base}/me"), get(me))
        .route(&format!("{base}/applications"), get(list).post(create))
        .route(&format!("{base}/applications/{{id}}"), get(describe).patch(modify).delete(remove))
        .route(&format!("{base}/applications/{{id}}/rollouts"), post(rollout))
        .route(&format!("{base}/registries/{{domain}}/credentials"), post(credentials))
        .route("/registry/token", get(registry_token))
}

fn config(state: &State) -> ApiResult<&ContainersConfig> {
    state.containers.as_ref().ok_or_else(|| {
        ApiError::new(
            http::StatusCode::BAD_REQUEST,
            10300,
            "Containers are not enabled on this Selvedge installation (the API has no registry configured)",
        )
    })
}

fn not_found() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 10301, "Application not found")
}

/// Every Durable Object namespace of the account: `(id, script, class)`,
/// from the classes its scripts' current versions bind.
pub(super) async fn do_namespaces(
    state: &State,
    account: &AccountId,
) -> ApiResult<Vec<(String, String, String)>> {
    let mut out = std::collections::BTreeSet::new();
    for script in state.db.scripts(account).await? {
        for b in state.db.current_bindings(account, &script.id).await?.unwrap_or_default() {
            if let Binding::DurableObjectNamespace { class_name, script_name, .. } = b {
                let host = script_name.unwrap_or_else(|| script.id.clone());
                out.insert((do_namespace_id(account.as_str(), &host, &class_name), host, class_name));
            }
        }
    }
    Ok(out.into_iter().collect())
}

fn app_json(a: &ContainerApp) -> Value {
    let mut v = a.spec.clone();
    v["id"] = json!(a.id);
    v["account_id"] = json!(a.account_id);
    v["name"] = json!(a.name);
    v["created_at"] = json!(a.created_at);
    v["version"] = json!(a.version);
    v["instances"] = json!(0);
    v["max_instances"] = json!(a.max_instances);
    v["durable_objects"] = json!({ "namespace_id": a.namespace_id });
    v["configuration"]["image"] = json!(a.image);
    v
}

/// `patch` merged into `base`: objects merge key by key, anything else
/// replaces; `null` leaves the value as it was.
fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (_, Value::Null) => {}
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// Checks an application and extracts what Selvedge acts on. `class` is the
/// Durable Object class it belongs to: `(namespace id, script, class)`.
fn fields(
    cfg: &ContainersConfig,
    account: &AccountId,
    name: &str,
    class: (String, String, String),
    spec: Value,
) -> ApiResult<ContainerAppFields> {
    let bad = |m: String| ApiError::new(http::StatusCode::BAD_REQUEST, 10302, m);
    if name.is_empty() || name.len() > 64 {
        return Err(bad("application names are 1-64 characters".into()));
    }
    let configuration = spec.get("configuration").cloned().unwrap_or(Value::Null);
    let image = configuration
        .get("image")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("`configuration.image` is required".into()))?
        .to_owned();
    // Only the installation's own registry, and only the account's part of
    // it: nodes pull with credentials limited to that prefix.
    let own = split_image(&image).is_some_and(|(host, repo, _)| {
        host == cfg.registry_host && repo.strip_prefix(account.as_str()).is_some_and(|r| r.starts_with('/'))
    });
    if !own {
        return Err(bad(format!(
            "image `{image}` is not in this account's registry namespace; push it to {}/{account}/<name> \
             (set CLOUDFLARE_CONTAINER_REGISTRY={} for wrangler)",
            cfg.registry_host, cfg.registry_host
        )));
    }
    let resources = resources_of(&configuration).map_err(bad)?;
    let max = &cfg.max_instance;
    if resources.vcpu > max.vcpu || resources.memory_mib > max.memory_mib || resources.disk_mb > max.disk_mb {
        return Err(bad(format!(
            "the instance asks for {} vCPU, {} MiB of memory and {} MB of disk; the limits are {}, {} and {}",
            resources.vcpu, resources.memory_mib, resources.disk_mb, max.vcpu, max.memory_mib, max.disk_mb
        )));
    }
    let max_instances = match spec.get("max_instances") {
        None | Some(Value::Null) => DEFAULT_MAX_INSTANCES,
        Some(v) => v
            .as_i64()
            .filter(|n| (0..=100_000).contains(n))
            .ok_or_else(|| bad("`max_instances` must be a number from 0 to 100000".into()))?,
    };
    let (namespace_id, script_name, class_name) = class;
    Ok(ContainerAppFields {
        name: name.to_owned(),
        namespace_id,
        script_name,
        class_name,
        image,
        max_instances: max_instances as i32,
        resources,
        spec,
    })
}

/// The account as the containers API describes it: `wrangler` checks an
/// image and an instance against these limits before it pushes.
async fn me(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let max = config(&state)?.max_instance;
    Ok(ok(json!({
        "external_account_id": id.as_str(),
        "legacy_identity": id.as_str(),
        "limits": {
            "account_id": id.as_str(),
            "vcpu_per_deployment": max.vcpu,
            "memory_mib_per_deployment": max.memory_mib,
            "memory_per_deployment": format!("{}MiB", max.memory_mib),
            "disk_mb_per_deployment": max.disk_mb,
            "disk_per_deployment": format!("{}MB", max.disk_mb),
            "total_vcpu": max.vcpu * 100.0,
            "total_memory_mib": max.memory_mib * 100,
            "node_group": "selvedge",
            "ipv4s": 0,
            "network_modes": ["private"],
            "total_ipv4s": 0,
        },
        "locations": [],
        "defaults": { "vcpus": 0.0625, "memory_mib": 256, "memory": "256MiB", "disk_mb": 2000 },
    })))
}

#[derive(Deserialize)]
struct ListQuery {
    name: Option<String>,
    image: Option<String>,
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let apps: Vec<Value> = state
        .db
        .container_apps(&id)
        .await?
        .iter()
        .filter(|a| q.name.as_ref().is_none_or(|n| *n == a.name))
        .filter(|a| q.image.as_ref().is_none_or(|i| *i == a.image))
        .map(app_json)
        .collect();
    Ok(ok(json!(apps)))
}

async fn create(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    JsonBody(spec): JsonBody<Value>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let cfg = config(&state)?;
    let bad = |m: &str| ApiError::new(http::StatusCode::BAD_REQUEST, 10302, m);
    if !spec.is_object() {
        return Err(bad("the application must be a JSON object"));
    }
    let name = spec.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
    // Containers without a Durable Object (Cloudchamber deployments) are not
    // supported: an instance exists only as some object's container.
    let namespace = spec
        .pointer("/durable_objects/namespace_id")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("`durable_objects.namespace_id` is required: an application belongs to a Durable Object class"))?;
    let class = do_namespaces(&state, &id).await?.into_iter().find(|(ns, _, _)| ns == namespace).ok_or_else(
        || bad("the Durable Object namespace was not found; deploy the Worker that binds the class first"),
    )?;
    let f = fields(cfg, &id, &name, class, spec)?;
    let app = state.db.create_container_app(&id, &f).await?.ok_or_else(|| {
        ApiError::new(
            http::StatusCode::CONFLICT,
            10303,
            "an application with this name, or for this Durable Object class, already exists",
        )
    })?;
    Ok(ok(app_json(&app)))
}

async fn describe(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, app)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let app = state.db.container_app(&id, &app).await?.ok_or_else(not_found)?;
    Ok(ok(app_json(&app)))
}

/// Applies `patch` to the stored application under `at` (the whole
/// application, or its `configuration`).
async fn apply(
    state: &State,
    account: &AccountId,
    app: &str,
    at: Option<&str>,
    patch: &Value,
) -> ApiResult<ContainerApp> {
    let cfg = config(state)?;
    let current = state.db.container_app(account, app).await?.ok_or_else(not_found)?;
    let mut spec = current.spec.clone();
    match at {
        Some(key) => merge(&mut spec[key], patch),
        None => merge(&mut spec, patch),
    }
    let class = (current.namespace_id.clone(), current.script_name.clone(), current.class_name.clone());
    let f = fields(cfg, account, &current.name, class, spec)?;
    state.db.update_container_app(account, app, &f).await?.ok_or_else(not_found)
}

async fn modify(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, app)): Path<(String, String)>,
    JsonBody(mut patch): JsonBody<Value>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    // What an application is and belongs to does not change.
    if let Some(o) = patch.as_object_mut() {
        for k in ["name", "durable_objects", "id", "account_id"] {
            o.remove(k);
        }
    }
    Ok(ok(app_json(&apply(&state, &id, &app, None, &patch).await?)))
}

async fn remove(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, app)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let cfg = config(&state)?;
    let Some(existing) = state.db.container_app(&id, &app).await? else { return Err(not_found()) };
    if !state.db.delete_container_app(&id, &app).await? {
        return Err(not_found());
    }
    // Its images go too, unless another application uses the repository.
    let repository = split_image(&existing.image)
        .filter(|(host, _, _)| *host == cfg.registry_host)
        .map(|(_, repo, _)| repo.to_owned());
    if let (Some(repository), Some(_)) = (repository, &cfg.signer) {
        let shared = state.db.container_apps(&id).await?.iter().any(|a| {
            split_image(&a.image).is_some_and(|(h, r, _)| h == cfg.registry_host && r == repository)
        });
        if !shared {
            let state = state.clone();
            tokio::spawn(async move {
                let Some(cfg) = state.containers.as_ref() else { return };
                let Some(signer) = cfg.signer.as_ref() else { return };
                match crate::registry::delete_repository(
                    &state.http,
                    &cfg.registry_url,
                    signer,
                    &id,
                    &repository,
                )
                .await
                {
                    Ok(n) => {
                        tracing::info!(account = %id, %repository, manifests = n, "deleted a removed application's images")
                    }
                    Err(e) => {
                        tracing::warn!(account = %id, %repository, error = %format!("{e:#}"), "deleting a removed application's images failed")
                    }
                }
            });
        }
    }
    Ok(ok(Value::Null))
}

/// A rollout: its target configuration becomes the application's, at once.
async fn rollout(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, app)): Path<(String, String)>,
    JsonBody(req): JsonBody<Value>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let target = req.get("target_configuration").cloned().unwrap_or(Value::Null);
    let updated = apply(&state, &id, &app, Some("configuration"), &target).await?;
    let (rollout_id, created) = state.db.record_container_rollout(&updated.id, &req).await?;
    Ok(ok(json!({
        "id": rollout_id,
        "created_at": created,
        "last_updated_at": created,
        "description": req.get("description").cloned().unwrap_or(json!("")),
        "status": "completed",
        "kind": req.get("kind").cloned().unwrap_or(json!("full_auto")),
        "strategy": req.get("strategy").cloned().unwrap_or(json!("rolling")),
        "current_version": updated.version,
        "target_version": updated.version,
        "target_configuration": updated.spec["configuration"],
        "health": { "instances": { "healthy": 0, "failed": 0, "starting": 0, "scheduling": 0 } },
        "steps": [],
        "progress": { "total_steps": 0, "current_step": 0, "updated_instances": 0, "total_instances": 0 },
    })))
}

#[derive(Deserialize)]
struct CredentialsRequest {
    #[serde(default)]
    expiration_minutes: Option<u64>,
    #[serde(default)]
    permissions: Vec<String>,
}

/// A short-lived registry password for the account (`docker login`).
async fn credentials(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, domain)): Path<(String, String)>,
    JsonBody(req): JsonBody<CredentialsRequest>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let cfg = config(&state)?;
    if domain != cfg.registry_host {
        return Err(ApiError::new(
            http::StatusCode::NOT_FOUND,
            10304,
            format!(
                "this installation's registry is {}, not {domain} (set CLOUDFLARE_CONTAINER_REGISTRY for wrangler)",
                cfg.registry_host
            ),
        ));
    }
    let permissions = Permissions {
        pull: req.permissions.iter().any(|p| p == "pull"),
        push: req.permissions.iter().any(|p| p == "push"),
    };
    let minutes = req.expiration_minutes.unwrap_or(15).clamp(1, MAX_CREDENTIAL_MINUTES);
    let password = cfg.credentials.issue(&id, permissions, crate::registry::now() + minutes * 60);
    Ok(ok(json!({
        "account_id": id.as_str(),
        "registry_host": cfg.registry_host,
        "username": USERNAME,
        "password": password,
    })))
}

/// The registry's token service (Docker's token authentication): Basic
/// credentials in, a signed token for the scopes they cover out. Without
/// valid credentials the answer is 401, which makes `docker login` fail.
async fn registry_token(AxState(state): AxState<State>, headers: HeaderMap, uri: http::Uri) -> Response {
    let unauthorized = || {
        (
            http::StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Basic realm=\"selvedge registry\"")],
            axum::Json(
                json!({ "errors": [{ "code": "UNAUTHORIZED", "message": "authentication required" }] }),
            ),
        )
            .into_response()
    };
    let Some(cfg) = state.containers.as_ref() else { return unauthorized() };
    let Some(signer) = cfg.signer.as_ref() else { return unauthorized() };
    let password = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.trim()).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|pair| pair.split_once(':').map(|(_, p)| p.to_owned()));
    let now = crate::registry::now();
    let Some((account, permissions)) = password.and_then(|p| cfg.credentials.verify(&p, now)) else {
        return unauthorized();
    };
    // `scope` repeats, once per repository.
    let scopes: Vec<String> = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .filter(|(k, _)| k == "scope")
        .map(|(_, v)| v.into_owned())
        .collect();
    let token = signer.token(&account, permissions, &scopes, now);
    axum::Json(json!({
        "token": token,
        "access_token": token,
        "expires_in": TokenSigner::ttl(),
        "issued_at": chrono::DateTime::from_timestamp(now as i64, 0).map(|t| t.to_rfc3339()),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ContainersConfig {
        ContainersConfig {
            registry_host: "registry.test".into(),
            registry_url: "https://registry.test".into(),
            signer: None,
            credentials: Credentials::new("t"),
            max_instance: Resources { vcpu: 1.0, memory_mib: 1024, disk_mb: 4000 },
        }
    }

    fn account() -> AccountId {
        "0123456789abcdef0123456789abcdef".parse().unwrap()
    }

    fn class() -> (String, String, String) {
        ("ns".into(), "script".into(), "Box".into())
    }

    #[test]
    fn images_must_be_the_accounts_own() {
        let ok =
            json!({ "configuration": { "image": format!("registry.test/{}/app@sha256:00", account()) } });
        let f = fields(&cfg(), &account(), "app", class(), ok).unwrap();
        assert_eq!(f.max_instances, 20);
        assert_eq!(f.resources.memory_mib, 256);
        for image in [
            "docker.io/library/nginx:latest".to_owned(),
            "registry.test/ffffffffffffffffffffffffffffffff/app:1".to_owned(),
            format!("registry.test/{}x/app:1", account()),
            format!("other.test/{}/app:1", account()),
            "nginx".to_owned(),
        ] {
            let spec = json!({ "configuration": { "image": image } });
            assert!(fields(&cfg(), &account(), "app", class(), spec).is_err(), "{image}");
        }
    }

    #[test]
    fn instances_are_limited() {
        let image = format!("registry.test/{}/app:1", account());
        let spec = json!({ "configuration": { "image": image, "instance_type": "standard-4" } });
        assert!(fields(&cfg(), &account(), "app", class(), spec).is_err());
        let spec =
            json!({ "configuration": { "image": image, "instance_type": "basic" }, "max_instances": 3 });
        assert_eq!(fields(&cfg(), &account(), "app", class(), spec).unwrap().max_instances, 3);
    }

    #[test]
    fn merging() {
        let mut v = json!({ "configuration": { "image": "a", "instance_type": "lite" }, "max_instances": 1 });
        merge(&mut v, &json!({ "configuration": { "image": "b" }, "max_instances": null, "x": [1] }));
        assert_eq!(
            v,
            json!({ "configuration": { "image": "b", "instance_type": "lite" }, "max_instances": 1, "x": [1] })
        );
    }
}
