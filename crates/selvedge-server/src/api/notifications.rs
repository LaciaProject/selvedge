//! R2 event notifications, as `wrangler r2 bucket notification` manages them:
//! rules that send a message to a queue when objects are created or deleted.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State as AxState};
use axum::response::Response;
use axum::routing::get;
use selvedge_core::AccountId;
use selvedge_store::r2notify::QueueRules;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, State, account_access};
use crate::error::{ApiError, ApiResult, ok};

/// The actions a rule may name, as Cloudflare names them.
const ACTIONS: &[&str] =
    &["PutObject", "CopyObject", "CompleteMultipartUpload", "DeleteObject", "LifecycleDeletion"];

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/event_notifications/r2/{bucket}/configuration";
    Router::new()
        .route(base, get(list))
        .route(&format!("{base}/queues/{{queue}}"), get(get_queue).put(put_rules).delete(delete_rules))
}

fn queue_json(q: &QueueRules) -> Value {
    json!({
        "queueId": q.queue_id,
        "queueName": q.queue_name,
        "rules": q.rules.iter().map(|r| json!({
            "ruleId": r.id,
            "prefix": r.prefix,
            "suffix": r.suffix,
            "actions": r.actions,
            "description": r.description,
            "createdAt": super::r2::iso(r.created_at),
        })).collect::<Vec<_>>(),
    })
}

/// The account's queue by id (or name), or 404.
async fn queue_id(state: &State, account: &AccountId, queue: &str) -> ApiResult<String> {
    Ok(state
        .db
        .queue(account, queue)
        .await?
        .ok_or_else(|| ApiError::new(http::StatusCode::NOT_FOUND, 11000, "The queue does not exist."))?
        .id)
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = super::r2::bucket_id(&state, &id, &bucket).await?;
    let queues = state.db.r2_notification_rules(&bucket_id, None).await?;
    Ok(ok(json!({ "bucketName": bucket, "queues": queues.iter().map(queue_json).collect::<Vec<_>>() })))
}

async fn get_queue(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, queue)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = super::r2::bucket_id(&state, &id, &bucket).await?;
    let queue = queue_id(&state, &id, &queue).await?;
    let found = state.db.r2_notification_rules(&bucket_id, Some(&queue)).await?;
    match found.first() {
        Some(q) => Ok(ok(queue_json(q))),
        None => {
            Err(ApiError::new(http::StatusCode::NOT_FOUND, 11001, "The bucket has no rules for this queue."))
        }
    }
}

#[derive(Deserialize)]
struct RuleBody {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    actions: Vec<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct PutBody {
    rules: Vec<RuleBody>,
}

async fn put_rules(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, queue)): Path<(String, String, String)>,
    super::JsonBody(body): super::JsonBody<PutBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = super::r2::bucket_id(&state, &id, &bucket).await?;
    let queue = queue_id(&state, &id, &queue).await?;
    if body.rules.is_empty() {
        return Err(ApiError::bad_request("give at least one rule"));
    }
    let mut rules = Vec::new();
    for r in body.rules {
        if r.actions.is_empty() {
            return Err(ApiError::bad_request("a rule needs at least one action"));
        }
        if let Some(a) = r.actions.iter().find(|a| !ACTIONS.contains(&a.as_str())) {
            return Err(ApiError::bad_request(format!(
                "`{a}` is not an action; use one of {}",
                ACTIONS.join(", ")
            )));
        }
        rules.push((r.prefix, r.suffix, r.actions, r.description));
    }
    let ids = state.db.add_r2_notification_rules(&bucket_id, &queue, &rules).await?;
    Ok(ok(json!({ "ruleIds": ids })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteBody {
    rule_ids: Option<Vec<String>>,
}

async fn delete_rules(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, queue)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = super::r2::bucket_id(&state, &id, &bucket).await?;
    let queue = queue_id(&state, &id, &queue).await?;
    // The body is optional: without `ruleIds`, every rule for the queue goes.
    let rule_ids = if body.is_empty() {
        None
    } else {
        serde_json::from_slice::<DeleteBody>(&body)
            .map_err(|e| ApiError::bad_request(e.to_string()))?
            .rule_ids
    };
    state.db.delete_r2_notification_rules(&bucket_id, &queue, rule_ids.as_deref()).await?;
    Ok(ok(Value::Null))
}
