//! Worker scripts: upload, list, delete, secrets and workers.dev settings.

use axum::Router;
use axum::extract::{Multipart, Path, State as AxState};
use axum::response::Response;
use axum::routing::{get, put};
use bytes::Bytes;
use selvedge_core::{AccountId, Binding, Module, ModuleKind, ScriptMetadata, validate_script_name};
use selvedge_store::{NewVersion, ScriptInfo};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, State, account_access};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/client/v4/accounts/{account}/workers/scripts", get(list))
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}",
            put(upload).get(content).delete(delete),
        )
        .route("/client/v4/accounts/{account}/workers/scripts/{script}/content", get(content))
        .route("/client/v4/accounts/{account}/workers/services/{script}", get(service).delete(delete))
        .route("/client/v4/accounts/{account}/workers/workers/{script}", get(worker))
        .route("/client/v4/accounts/{account}/workers/scripts/{script}/references", get(references))
        .route("/client/v4/accounts/{account}/workers/tails/by-consumer/{script}", get(tail_producers))
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}/settings",
            get(settings).patch(settings),
        )
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}/secrets",
            get(list_secrets).put(put_secret),
        )
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}/secrets/{name}",
            get(get_secret).delete(delete_secret),
        )
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}/subdomain",
            get(get_script_subdomain).post(set_script_subdomain),
        )
        .route("/client/v4/accounts/{account}/workers/subdomain", get(get_subdomain).put(put_subdomain))
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}/schedules",
            get(schedules).put(put_schedules),
        )
        .route("/client/v4/accounts/{account}/workers/durable_objects/namespaces", get(do_namespaces))
}

pub(super) fn script_json(s: &ScriptInfo) -> Value {
    json!({
        "id": s.id,
        "etag": s.current_version_id,
        "created_on": s.created_on,
        "modified_on": s.modified_on,
        "usage_model": "standard",
        "handlers": ["fetch"],
        "has_modules": true,
        "has_assets": false,
    })
}

pub(super) async fn script_access(
    state: &State,
    caller: &Caller,
    account: &str,
    script: &str,
) -> ApiResult<AccountId> {
    let id = account_access(state, caller, account).await?;
    validate_script_name(script).map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(id)
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let scripts = state.db.scripts(&id).await?;
    Ok(ok(json!(scripts.iter().map(script_json).collect::<Vec<_>>())))
}

async fn service(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let s = state.db.script(&id, &script).await?.ok_or_else(|| {
        ApiError::new(http::StatusCode::NOT_FOUND, 10090, "This Worker does not exist on your account.")
    })?;
    Ok(ok(json!({
        "id": s.id,
        "default_environment": {
            "environment": "production",
            "created_on": s.created_on,
            "modified_on": s.modified_on,
            "script": script_json(&s),
        },
        "created_on": s.created_on,
        "modified_on": s.modified_on,
    })))
}

/// What depends on a script: service bindings and Durable Object bindings from
/// other scripts in the account. `wrangler delete` checks this.
async fn references(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let mut incoming = Vec::new();
    let mut durable_objects = Vec::new();
    for other in state.db.scripts(&id).await? {
        if other.id == script {
            continue;
        }
        for b in state.db.current_bindings(&id, &other.id).await?.unwrap_or_default() {
            match b {
                Binding::Service { name, service, .. } if service == script => {
                    incoming.push(json!({ "service": other.id, "environment": "production", "name": name }));
                }
                Binding::DurableObjectNamespace { class_name, script_name: Some(host), .. }
                    if host == script =>
                {
                    durable_objects.push(
                        json!({ "service": other.id, "environment": "production", "class_name": class_name }),
                    );
                }
                _ => {}
            }
        }
    }
    Ok(ok(json!({
        "services": { "incoming": incoming, "outgoing": [], "pages_function": false },
        "durable_objects": durable_objects,
        "dispatch_outbounds": [],
    })))
}

/// Tail Workers are not supported, so nothing tails into a script.
async fn tail_producers(caller: Caller, Path((_account, _script)): Path<(String, String)>) -> Response {
    let _ = caller;
    ok(json!([]))
}

/// The newer Workers resource (`/workers/workers/:name`).
async fn worker(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let s = state.db.script(&id, &script).await?.ok_or_else(|| {
        ApiError::new(http::StatusCode::NOT_FOUND, 10007, "This Worker does not exist on your account.")
    })?;
    Ok(ok(json!({
        "id": s.id,
        "name": s.id,
        "created_on": s.created_on,
        "updated_on": s.modified_on,
        "subdomain": { "enabled": s.workers_dev, "previews_enabled": false },
        "logpush": false,
        "observability": { "enabled": false },
        "tags": [],
        "tail_consumers": [],
    })))
}

/// Parses a Cloudflare script upload: a `metadata` JSON part and one part per
/// module. Returns the metadata and `(name, kind, bytes)` per module.
pub(super) async fn read_upload(
    mut form: Multipart,
) -> ApiResult<(ScriptMetadata, Vec<(String, ModuleKind, Bytes)>)> {
    let mut metadata = None;
    let mut modules = Vec::new();
    while let Some(field) = form.next_field().await.map_err(|e| ApiError::bad_request(e.to_string()))? {
        let name = field.name().unwrap_or_default().to_owned();
        let file_name = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let bytes = field.bytes().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
        if name == "metadata" {
            metadata = Some(
                serde_json::from_slice::<ScriptMetadata>(&bytes)
                    .map_err(|e| ApiError::bad_request(format!("invalid metadata: {e}")))?,
            );
            continue;
        }
        let module_name = file_name.unwrap_or(name);
        let kind =
            ModuleKind::from_content_type(content_type.as_deref().unwrap_or("application/octet-stream"))
                .map_err(ApiError::bad_request)?;
        if let Some(kind) = kind {
            if modules.iter().any(|(n, _, _): &(String, _, _)| *n == module_name) {
                return Err(ApiError::bad_request(format!("duplicate module `{module_name}`")));
            }
            modules.push((module_name, kind, bytes));
        }
    }
    let metadata = metadata.ok_or_else(|| ApiError::bad_request("missing `metadata` part"))?;
    Ok((metadata, modules))
}

async fn upload(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    form: Multipart,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let (version, info, meta) = create_version(&state, &id, &script, form, true).await?;
    let mut result = script_json(&info);
    result["compatibility_date"] = json!(meta.compatibility_date);
    result["compatibility_flags"] = json!(meta.compatibility_flags);
    result["deployment_id"] = json!(version);
    result["startup_time_ms"] = json!(0);
    Ok(ok(result))
}

/// Stores an uploaded version; `make_current` deploys it.
pub(super) async fn create_version(
    state: &State,
    id: &AccountId,
    script: &str,
    form: Multipart,
    make_current: bool,
) -> ApiResult<(selvedge_core::VersionId, ScriptInfo, ScriptMetadata)> {
    let id = id.clone();
    let (mut meta, parts) = read_upload(form).await?;
    if meta.body_part.is_some() && meta.main_module.is_none() {
        return Err(ApiError::bad_request(
            "service-worker syntax is not supported; use ES modules (`export default { fetch }`)",
        ));
    }
    let main_module =
        meta.main_module.clone().ok_or_else(|| ApiError::bad_request("metadata is missing `main_module`"))?;
    if !parts.iter().any(|(n, _, _)| *n == main_module) {
        return Err(ApiError::bad_request(format!("main module `{main_module}` was not uploaded")));
    }
    let compatibility_date = meta.compatibility_date.clone().unwrap_or_else(|| "2024-01-01".to_owned());
    if !is_date(&compatibility_date) {
        return Err(ApiError::bad_request("compatibility_date must be YYYY-MM-DD"));
    }

    let mut bindings = Vec::new();
    let mut secrets = Vec::new();
    for raw in &meta.bindings {
        let mut b = Binding::parse(raw).map_err(ApiError::bad_request)?;
        if bindings.iter().any(|x: &Binding| x.name() == b.name()) {
            return Err(ApiError::bad_request(format!("duplicate binding `{}`", b.name())));
        }
        match &mut b {
            Binding::SecretText { name, text } => {
                if let Some(t) = text.take() {
                    secrets.push((name.clone(), t));
                }
            }
            Binding::KvNamespace { namespace_id, .. } => {
                if state.db.kv_namespace_owned(&id, namespace_id).await?.is_none() {
                    return Err(ApiError::bad_request(format!("KV namespace `{namespace_id}` not found")));
                }
            }
            Binding::Service { service, .. } => {
                validate_script_name(service).map_err(|e| ApiError::bad_request(e.to_string()))?;
            }
            Binding::D1 { id: db, .. } => {
                if state.db.d1_database(&id, db).await?.is_none_or(|d| d.id != *db) {
                    return Err(ApiError::bad_request(format!("D1 database `{db}` not found")));
                }
            }
            Binding::R2Bucket { bucket_name, .. } => {
                if state.db.r2_bucket(&id, bucket_name).await?.is_none() {
                    return Err(ApiError::bad_request(format!("R2 bucket `{bucket_name}` not found")));
                }
            }
            _ => {}
        }
        bindings.push(b);
    }

    let mut modules = Vec::new();
    for (name, kind, bytes) in parts {
        let sha256 = state.blobs.put(bytes).await?;
        modules.push(Module { name, kind, sha256 });
    }
    let (version, info) = state
        .db
        .put_version(
            &state.secrets,
            &id,
            script,
            NewVersion {
                main_module,
                compatibility_date: compatibility_date.clone(),
                compatibility_flags: meta.compatibility_flags.clone(),
                modules,
                bindings,
                keep_bindings: meta.keep_bindings.clone(),
                secrets,
            },
            make_current,
        )
        .await?;
    meta.compatibility_date = Some(compatibility_date);
    Ok((version, info, meta))
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// The current version as multipart, as Cloudflare returns it.
async fn content(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let (main, modules) = state.db.current_modules(&id, &script).await?.ok_or_else(|| {
        ApiError::new(http::StatusCode::NOT_FOUND, 10007, "This Worker does not exist on your account.")
    })?;
    let boundary = format!("selvedge-{}", selvedge_core::HexId::random());
    let mut body = Vec::new();
    for m in &modules {
        let bytes =
            state.blobs.get(&m.sha256).await?.ok_or_else(|| anyhow::anyhow!("blob {} missing", m.sha256))?;
        let ct = match m.kind {
            ModuleKind::EsModule => "application/javascript+module",
            ModuleKind::CommonJs => "application/javascript",
            ModuleKind::Text => "text/plain",
            ModuleKind::Data => "application/octet-stream",
            ModuleKind::Wasm => "application/wasm",
            ModuleKind::Json => "application/json",
            ModuleKind::Python => "text/x-python",
        };
        body.extend(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{0}\"; filename=\"{0}\"\r\nContent-Type: {ct}\r\n\r\n",
                m.name
            )
            .as_bytes(),
        );
        body.extend(&bytes);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").as_bytes());
    Ok(Response::builder()
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .header("cf-entrypoint", main)
        .body(axum::body::Body::from(body))
        .map_err(anyhow::Error::from)?)
}

async fn delete(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if !state.db.delete_script(&id, &script).await? {
        return Err(ApiError::new(
            http::StatusCode::NOT_FOUND,
            10007,
            "This Worker does not exist on your account.",
        ));
    }
    Ok(ok(json!({ "id": script })))
}

async fn settings(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let bindings = state
        .db
        .current_bindings(&id, &script)
        .await?
        .ok_or_else(|| ApiError::not_found("This Worker does not exist on your account."))?;
    Ok(ok(json!({ "bindings": bindings, "logpush": false, "tail_consumers": [] })))
}

#[derive(Deserialize)]
struct SecretBody {
    name: String,
    text: String,
    #[serde(rename = "type")]
    ty: Option<String>,
}

async fn put_secret(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    super::JsonBody(body): super::JsonBody<SecretBody>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if body.ty.as_deref().is_some_and(|t| t != "secret_text") {
        return Err(ApiError::bad_request("only `secret_text` secrets are supported"));
    }
    if body.name.is_empty() || body.name.len() > 255 {
        return Err(ApiError::bad_request("secret name must be 1-255 characters"));
    }
    if !state.db.put_secret(&state.secrets, &id, &script, &body.name, &body.text).await? {
        return Err(ApiError::new(
            http::StatusCode::NOT_FOUND,
            10007,
            "This Worker does not exist on your account.",
        ));
    }
    Ok(ok(json!({ "name": body.name, "type": "secret_text" })))
}

async fn list_secrets(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let names = state.db.secret_names(&id, &script).await?;
    Ok(ok(json!(names.iter().map(|n| json!({ "name": n, "type": "secret_text" })).collect::<Vec<_>>())))
}

async fn get_secret(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script, name)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if state.db.secret_names(&id, &script).await?.contains(&name) {
        Ok(ok(json!({ "name": name, "type": "secret_text" })))
    } else {
        Err(ApiError::not_found("Secret not found"))
    }
}

async fn delete_secret(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script, name)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if !state.db.delete_secret(&state.secrets, &id, &script, &name).await? {
        return Err(ApiError::not_found("Secret not found"));
    }
    Ok(ok(Value::Null))
}

async fn get_subdomain(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let a = state.db.account(&id).await?.ok_or_else(|| ApiError::not_found("Account not found"))?;
    match a.subdomain {
        Some(s) => Ok(ok(json!({ "subdomain": s }))),
        None => Err(ApiError::new(http::StatusCode::NOT_FOUND, 10007, "Subdomain not set")),
    }
}

#[derive(Deserialize)]
struct SubdomainBody {
    subdomain: String,
}

async fn put_subdomain(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    super::JsonBody(body): super::JsonBody<SubdomainBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let s = body.subdomain.to_ascii_lowercase();
    let valid = !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-');
    if !valid {
        return Err(ApiError::bad_request("invalid subdomain"));
    }
    if !state.db.set_subdomain(&id, &s).await? {
        return Err(ApiError::new(http::StatusCode::CONFLICT, 10036, "Subdomain is not available"));
    }
    Ok(ok(json!({ "subdomain": s })))
}

async fn get_script_subdomain(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let s = state
        .db
        .script(&id, &script)
        .await?
        .ok_or_else(|| ApiError::not_found("This Worker does not exist on your account."))?;
    Ok(ok(json!({ "enabled": s.workers_dev, "previews_enabled": false })))
}

#[derive(Deserialize)]
struct ScriptSubdomainBody {
    enabled: bool,
}

async fn set_script_subdomain(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    super::JsonBody(body): super::JsonBody<ScriptSubdomainBody>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if !state.db.set_workers_dev(&id, &script, body.enabled).await? {
        return Err(ApiError::not_found("This Worker does not exist on your account."));
    }
    Ok(ok(json!({ "enabled": body.enabled, "previews_enabled": false })))
}

/// Cron triggers are not implemented yet: report none, and reject setting any.
async fn schedules(caller: Caller, Path((_account, _script)): Path<(String, String)>) -> Response {
    let _ = caller;
    ok(json!({ "schedules": [] }))
}

async fn put_schedules(
    caller: Caller,
    Path((_account, _script)): Path<(String, String)>,
    super::JsonBody(body): super::JsonBody<Value>,
) -> ApiResult<Response> {
    let _ = caller;
    if body.as_array().is_some_and(|a| !a.is_empty()) {
        return Err(ApiError::bad_request("Cron Triggers are not supported by Selvedge yet"));
    }
    Ok(ok(json!({ "schedules": [] })))
}

async fn do_namespaces(caller: Caller, Path(_account): Path<String>) -> Response {
    let _ = caller;
    ok(json!([]))
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert!(super::is_date("2026-11-06"));
        assert!(!super::is_date("2026-1-06"));
        assert!(!super::is_date("2026/11/06"));
    }
}
