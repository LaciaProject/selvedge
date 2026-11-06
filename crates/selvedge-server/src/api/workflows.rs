//! Workflows: the account-wide registry `wrangler deploy` writes, and the
//! instance API `wrangler workflows` uses. Instance calls go through the
//! gateway's internal routing to the account's `selvedge-workflows` system
//! worker, which talks to Cloudflare's engine.

use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use selvedge_core::model::valid_workflow_name;
use selvedge_core::{AccountId, validate_script_name};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, JsonBody, State, account_access};
use crate::agent::{ACCOUNT_HEADER, SCRIPT_HEADER, TOKEN_HEADER};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/workflows";
    Router::new()
        .route(base, get(list))
        .route(&format!("{base}/{{name}}"), get(describe).put(put).delete(remove))
        .route(&format!("{base}/{{name}}/instances"), post(create_instance).get(list_instances))
        // `wrangler workflows instances describe <name> latest` lists with a trailing slash.
        .route(&format!("{base}/{{name}}/instances/"), get(list_instances))
        .route(&format!("{base}/{{name}}/instances/batch/delete"), post(delete_instances))
        .route(&format!("{base}/{{name}}/instances/{{id}}"), get(instance))
        .route(&format!("{base}/{{name}}/instances/{{id}}/status"), patch(instance_status))
        .route(&format!("{base}/{{name}}/instances/{{id}}/events/{{event}}"), post(instance_event))
}

fn not_found() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 10200, "Workflow not found")
}

async fn workflow_access(state: &State, caller: &Caller, account: &str, name: &str) -> ApiResult<AccountId> {
    let id = account_access(state, caller, account).await?;
    if !valid_workflow_name(name) {
        return Err(ApiError::bad_request("workflow names are 1-64 characters of a-z, 0-9, '-' and '_'"));
    }
    Ok(id)
}

fn workflow_json(w: &selvedge_store::workflows::WorkflowInfo) -> Value {
    json!({
        "id": w.id,
        "name": w.name,
        "script_name": w.script_name,
        "class_name": w.class_name,
        "created_on": w.created_on,
        "modified_on": w.modified_on,
    })
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let all = state.db.workflows(&id).await?;
    Ok(ok(json!(all.iter().map(workflow_json).collect::<Vec<_>>())))
}

async fn describe(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    let w = state.db.workflow(&id, &name).await?.ok_or_else(not_found)?;
    Ok(ok(workflow_json(&w)))
}

#[derive(Deserialize)]
struct PutBody {
    script_name: String,
    class_name: String,
}

async fn put(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name)): Path<(String, String)>,
    JsonBody(body): JsonBody<PutBody>,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    validate_script_name(&body.script_name).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if body.class_name.is_empty()
        || !body.class_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return Err(ApiError::bad_request("class_name must be a JavaScript class name"));
    }
    let w = state.db.put_workflow(&id, &name, &body.script_name, &body.class_name).await?;
    Ok(ok(workflow_json(&w)))
}

async fn remove(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    if !state.db.delete_workflow(&id, &name).await? {
        return Err(not_found());
    }
    Ok(ok(json!({ "status": "ok", "success": true })))
}

/// Sends an operation to the account's `selvedge-workflows` worker. Returns
/// its status and JSON body.
async fn call(state: &State, account: &AccountId, op: &str, body: Value) -> ApiResult<(u16, Value)> {
    let gateway = state.gateway_url.as_deref().ok_or_else(|| {
        ApiError::new(
            http::StatusCode::SERVICE_UNAVAILABLE,
            7500,
            "Workflow instances need SELVEDGE_GATEWAY_URL",
        )
    })?;
    // The account's process may be starting, or picking up a new workflow.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let resp = state
            .http
            .post(format!("{gateway}/{op}"))
            .header(TOKEN_HEADER, &state.internal_token)
            .header(ACCOUNT_HEADER, account.as_str())
            .header(SCRIPT_HEADER, selvedge_workerd::render::WORKFLOWS_SERVICE)
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                let text = r.text().await.unwrap_or_default();
                let value: Option<Value> = serde_json::from_str(&text).ok();
                // Not ready yet: a 404 without the worker's JSON is the agent
                // (no such socket yet), and `workflow.not_found` from a
                // registered workflow is a process that predates it.
                let starting = status == 503
                    || (status == 404
                        && value.as_ref().is_none_or(|v| v["error"].as_str() == Some("workflow.not_found")));
                if starting && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                return Ok((status, value.unwrap_or_else(|| json!({ "error": text }))));
            }
            Err(e) if std::time::Instant::now() < deadline => {
                tracing::debug!(error = %e, "workflows call failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => return Err(anyhow::Error::from(e).into()),
        }
    }
}

/// Maps a worker error to the API's.
fn worker_error(status: u16, body: &Value) -> ApiError {
    let message = body["error"].as_str().unwrap_or("workflow error").to_owned();
    if status == 404 {
        let what = if message.contains("instance") { "Instance not found" } else { "Workflow not found" };
        return ApiError::new(http::StatusCode::NOT_FOUND, 10200, what);
    }
    ApiError::bad_request(message)
}

async fn registered(state: &State, account: &AccountId, name: &str) -> ApiResult<()> {
    state.db.workflow(account, name).await?.map(|_| ()).ok_or_else(not_found)
}

#[derive(Deserialize, Default)]
struct CreateBody {
    #[serde(alias = "id")]
    instance_id: Option<String>,
    params: Option<Value>,
}

async fn create_instance(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    registered(&state, &id, &name).await?;
    let body: CreateBody = if body.is_empty() {
        CreateBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };
    let (status, r) = call(
        &state,
        &id,
        "create",
        json!({ "workflow": name, "id": body.instance_id, "params": body.params.unwrap_or_else(|| json!({})) }),
    )
    .await?;
    if status != 200 {
        return Err(worker_error(status, &r));
    }
    Ok(ok(json!({ "id": r["id"], "status": "queued", "version_id": "", "workflow_id": name })))
}

#[derive(Deserialize)]
struct Page {
    page: Option<u64>,
    per_page: Option<u64>,
}

async fn list_instances(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name)): Path<(String, String)>,
    Query(q): Query<Page>,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    registered(&state, &id, &name).await?;
    let per_page = q.per_page.unwrap_or(50).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);
    let (status, r) = call(
        &state,
        &id,
        "list",
        json!({ "workflow": name, "offset": (page - 1) * per_page, "limit": per_page }),
    )
    .await?;
    if status != 200 {
        return Err(worker_error(status, &r));
    }
    let total = r["total"].as_u64().unwrap_or(0);
    let body = json!({
        "success": true, "errors": [], "messages": [],
        "result": r["instances"],
        "result_info": {
            "page": page, "per_page": per_page, "count": r["instances"].as_array().map_or(0, Vec::len),
            "total_count": total, "total_pages": total.div_ceil(per_page).max(1),
        },
    });
    Ok(axum::Json(body).into_response())
}

async fn instance(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name, instance)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    registered(&state, &id, &name).await?;
    let (status, r) = call(&state, &id, "details", json!({ "workflow": name, "id": instance })).await?;
    if status != 200 {
        return Err(worker_error(status, &r));
    }
    Ok(ok(r))
}

#[derive(Deserialize)]
struct DeleteBody {
    instances: Vec<String>,
}

async fn delete_instances(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name)): Path<(String, String)>,
    JsonBody(body): JsonBody<DeleteBody>,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    registered(&state, &id, &name).await?;
    let (status, r) =
        call(&state, &id, "delete", json!({ "workflow": name, "instances": body.instances })).await?;
    if status != 200 {
        return Err(worker_error(status, &r));
    }
    Ok(ok(json!({ "deleted": r["deleted"], "errors": r["errors"] })))
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
    #[serde(flatten)]
    options: serde_json::Map<String, Value>,
}

async fn instance_status(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name, instance)): Path<(String, String, String)>,
    JsonBody(body): JsonBody<StatusBody>,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    registered(&state, &id, &name).await?;
    let (status, r) = call(
        &state,
        &id,
        "status",
        json!({ "workflow": name, "id": instance, "status": body.status, "options": body.options }),
    )
    .await?;
    if status != 200 {
        return Err(worker_error(status, &r));
    }
    Ok(ok(json!({ "status": body.status, "timestamp": chrono::Utc::now().to_rfc3339() })))
}

async fn instance_event(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, name, instance, event)): Path<(String, String, String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let id = workflow_access(&state, &caller, &account, &name).await?;
    registered(&state, &id, &name).await?;
    let payload: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };
    let (status, r) = call(
        &state,
        &id,
        "event",
        json!({ "workflow": name, "id": instance, "type": event, "payload": payload }),
    )
    .await?;
    if status != 200 {
        return Err(worker_error(status, &r));
    }
    Ok(ok(Value::Null))
}
