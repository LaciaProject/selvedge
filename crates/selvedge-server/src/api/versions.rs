//! Worker versions and deployments, as `wrangler deploy`, `wrangler versions`
//! and `wrangler rollback` use them. Selvedge deploys one version at a time:
//! a deployment is the current version at 100%. Gradual deployments are
//! rejected rather than silently sent to one version.

use axum::Router;
use axum::extract::{Multipart, Path, State as AxState};
use axum::response::Response;
use axum::routing::get;
use selvedge_store::VersionInfo;
use serde::Deserialize;
use serde_json::{Value, json};

use super::scripts::{create_version, script_access};
use super::{Caller, State};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/workers/scripts/{script}";
    Router::new()
        .route(&format!("{base}/versions"), get(list).post(upload))
        .route(&format!("{base}/versions/{{version}}"), get(get_version))
        .route(&format!("{base}/deployments"), get(deployments).post(deploy))
        .route(&format!("{base}/script-settings"), get(script_settings).patch(script_settings))
}

fn worker_not_found() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 10007, "This Worker does not exist on your account.")
}

/// A version as the API describes it. Durable Object bindings carry their
/// namespace's id, which `wrangler` reads to attach a container application
/// to a class.
fn version_json(v: &VersionInfo, account: &selvedge_core::AccountId, script: &str) -> Value {
    let mut bindings = json!(v.bindings);
    for (b, out) in v.bindings.iter().zip(bindings.as_array_mut().into_iter().flatten()) {
        if let selvedge_core::Binding::DurableObjectNamespace { class_name, script_name, .. } = b {
            let host = script_name.as_deref().unwrap_or(script);
            out["namespace_id"] =
                json!(selvedge_core::containers::do_namespace_id(account.as_str(), host, class_name));
        }
    }
    json!({
        "id": v.id,
        "number": v.number,
        "metadata": {
            "created_on": v.created_on,
            "modified_on": v.created_on,
            "source": "api",
            "author_email": "",
            "hasPreview": false,
        },
        "annotations": { "workers/triggered_by": "upload" },
        "resources": {
            "bindings": bindings,
            "script": { "etag": v.id, "handlers": ["fetch"] },
            "script_runtime": {
                "compatibility_date": v.compatibility_date,
                "compatibility_flags": v.compatibility_flags,
                "usage_model": "standard",
            },
        },
    })
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if state.db.script(&id, &script).await?.is_none() {
        return Err(worker_not_found());
    }
    let versions = state.db.versions(&id, &script).await?;
    Ok(ok(json!({ "items": versions.iter().map(|v| version_json(v, &id, &script)).collect::<Vec<_>>() })))
}

async fn get_version(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script, version)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let v = state.db.version(&id, &script, &version).await?.ok_or_else(|| {
        ApiError::new(http::StatusCode::NOT_FOUND, 10220, "The specified version does not exist")
    })?;
    Ok(ok(version_json(&v, &id, &script)))
}

/// `wrangler versions upload`: a new version that is not deployed.
async fn upload(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    form: Multipart,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let (version, _, _) = create_version(&state, &id, &script, form, false).await?;
    let v = state.db.version(&id, &script, &version).await?.ok_or_else(worker_not_found)?;
    let mut out = version_json(&v, &id, &script);
    out["startup_time_ms"] = json!(0);
    Ok(ok(out))
}

async fn deployments(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let s = state.db.script(&id, &script).await?.ok_or_else(worker_not_found)?;
    let split = state.db.split(&id, &script).await?;
    let deployments: Vec<Value> = s
        .current_version_id
        .iter()
        .map(|v| {
            let versions = match &split {
                Some(parts) => {
                    parts.iter().map(|(v, p)| json!({ "version_id": v, "percentage": p })).collect::<Vec<_>>()
                }
                None => vec![json!({ "version_id": v, "percentage": 100.0 })],
            };
            json!({
                "id": v,
                "source": "api",
                "strategy": "percentage",
                "author_email": "",
                "created_on": s.modified_on,
                "annotations": { "workers/triggered_by": "deployment" },
                "versions": versions,
            })
        })
        .collect();
    Ok(ok(json!({ "deployments": deployments })))
}

#[derive(Deserialize)]
struct DeployBody {
    #[serde(default)]
    versions: Vec<DeployVersion>,
}

#[derive(Deserialize)]
struct DeployVersion {
    version_id: String,
    percentage: f64,
}

/// `wrangler versions deploy` and `wrangler rollback`.
async fn deploy(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    super::JsonBody(body): super::JsonBody<DeployBody>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let mut live: Vec<_> = body.versions.iter().filter(|v| v.percentage > 0.0).collect();
    let total: f64 = live.iter().map(|v| v.percentage).sum();
    if live.is_empty() || live.len() > 2 || (total - 100.0).abs() > 0.01 {
        return Err(ApiError::bad_request(
            "a deployment is one or two versions whose percentages add up to 100",
        ));
    }
    if live.len() == 2 {
        // The larger share is the current version.
        live.sort_by(|a, b| b.percentage.total_cmp(&a.percentage));
        for v in &live {
            let info = state.db.version(&id, &script, &v.version_id).await?.ok_or_else(|| {
                ApiError::new(http::StatusCode::NOT_FOUND, 10220, "The specified version does not exist")
            })?;
            let hosts_objects = info.bindings.iter().any(|b| {
                matches!(b, selvedge_core::Binding::DurableObjectNamespace { script_name, .. }
                    if script_name.as_deref().is_none_or(|s| s == script))
            });
            if hosts_objects || info.has_assets {
                return Err(ApiError::bad_request(
                    "gradual deployments of Workers that define Durable Objects or have static assets are not supported",
                ));
            }
        }
        let parts: Vec<(String, f64)> = live.iter().map(|v| (v.version_id.clone(), v.percentage)).collect();
        if !state.db.set_split(&id, &script, &parts).await? {
            return Err(ApiError::new(
                http::StatusCode::NOT_FOUND,
                10220,
                "The specified version does not exist",
            ));
        }
        return Ok(ok(json!({
            "id": parts[0].0,
            "source": "api",
            "strategy": "percentage",
            "versions": parts.iter().map(|(v, p)| json!({ "version_id": v, "percentage": p })).collect::<Vec<_>>(),
        })));
    }
    let only = live[0];
    if !state.db.set_current_version(&id, &script, &only.version_id).await? {
        return Err(ApiError::new(
            http::StatusCode::NOT_FOUND,
            10220,
            "The specified version does not exist",
        ));
    }
    Ok(ok(json!({
        "id": only.version_id,
        "source": "api",
        "strategy": "percentage",
        "versions": [{ "version_id": only.version_id, "percentage": 100.0 }],
    })))
}

async fn script_settings(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if state.db.script(&id, &script).await?.is_none() {
        return Err(worker_not_found());
    }
    Ok(ok(json!({ "logpush": false, "tail_consumers": [], "observability": { "enabled": false } })))
}
