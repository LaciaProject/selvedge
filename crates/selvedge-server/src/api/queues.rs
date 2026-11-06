//! The Queues REST API: queues and their (one) Worker consumer.

use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::response::Response;
use axum::routing::{get, put};
use selvedge_core::AccountId;
use selvedge_store::queues::{ConsumerSettings, QueueInfo};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, JsonBody, State, account_access};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/queues";
    Router::new()
        .route(base, get(list).post(create))
        .route(&format!("{base}/{{queue}}"), get(get_queue).delete(delete_queue))
        .route(&format!("{base}/{{queue}}/consumers"), get(list_consumers).post(add_consumer))
        .route(
            &format!("{base}/{{queue}}/consumers/{{consumer}}"),
            put(update_consumer).delete(delete_consumer).get(get_consumer),
        )
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn not_found() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 11000, "Queue does not exist")
}

fn consumer_json(q: &QueueInfo, c: &ConsumerSettings) -> Value {
    json!({
        "consumer_id": q.id,
        "queue_id": q.id,
        "queue_name": q.name,
        "script": c.script,
        "script_name": c.script,
        "service": c.script,
        "environment": "production",
        "type": "worker",
        "created_on": q.created_on,
        "settings": {
            "batch_size": c.batch_size,
            "max_retries": c.max_retries,
            "max_wait_time_ms": c.max_wait_ms,
            "retry_delay": c.retry_delay_secs,
        },
        "dead_letter_queue": c.dead_letter_queue,
    })
}

async fn queue_json(state: &State, q: &QueueInfo) -> ApiResult<Value> {
    let consumers: Vec<Value> = state.db.consumer(&q.id).await?.iter().map(|c| consumer_json(q, c)).collect();
    Ok(json!({
        "queue_id": q.id,
        "queue_name": q.name,
        "created_on": q.created_on,
        "modified_on": q.created_on,
        "producers": [],
        "producers_total_count": 0,
        "consumers": consumers,
        "consumers_total_count": consumers.len(),
        "settings": { "delivery_delay": 0, "message_retention_period": 345600 },
    }))
}

async fn queue_access(
    state: &State,
    caller: &Caller,
    account: &str,
    queue: &str,
) -> ApiResult<(AccountId, QueueInfo)> {
    let id = account_access(state, caller, account).await?;
    let q = state.db.queue(&id, queue).await?.ok_or_else(not_found)?;
    Ok((id, q))
}

#[derive(Deserialize)]
struct ListQuery {
    name: Option<String>,
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let mut out = Vec::new();
    for queue in state.db.queues(&id).await? {
        if q.name.as_ref().is_none_or(|n| *n == queue.name) {
            out.push(queue_json(&state, &queue).await?);
        }
    }
    Ok(ok(json!(out)))
}

#[derive(Deserialize)]
struct CreateBody {
    queue_name: String,
}

async fn create(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    JsonBody(body): JsonBody<CreateBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    if !valid_name(&body.queue_name) {
        return Err(ApiError::bad_request("queue names are 1-63 lowercase letters, digits, '-' and '_'"));
    }
    let q = state.db.create_queue(&id, &body.queue_name).await?.ok_or_else(|| {
        ApiError::new(http::StatusCode::CONFLICT, 11009, "A queue with this name already exists")
    })?;
    Ok(ok(queue_json(&state, &q).await?))
}

async fn get_queue(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (_, q) = queue_access(&state, &caller, &account, &queue).await?;
    Ok(ok(queue_json(&state, &q).await?))
}

async fn delete_queue(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (id, q) = queue_access(&state, &caller, &account, &queue).await?;
    state.db.delete_queue(&id, &q.id).await?;
    Ok(ok(Value::Null))
}

async fn list_consumers(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (_, q) = queue_access(&state, &caller, &account, &queue).await?;
    let list: Vec<Value> = state.db.consumer(&q.id).await?.iter().map(|c| consumer_json(&q, c)).collect();
    Ok(ok(json!(list)))
}

async fn get_consumer(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue, _consumer)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let (_, q) = queue_access(&state, &caller, &account, &queue).await?;
    let c = state.db.consumer(&q.id).await?.ok_or_else(|| ApiError::not_found("Consumer does not exist"))?;
    Ok(ok(consumer_json(&q, &c)))
}

#[derive(Deserialize, Default)]
struct ConsumerSettingsBody {
    batch_size: Option<i32>,
    max_retries: Option<i32>,
    max_wait_time_ms: Option<i32>,
    retry_delay: Option<i32>,
}

#[derive(Deserialize)]
struct ConsumerBody {
    script_name: Option<String>,
    script: Option<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
    #[serde(default)]
    settings: ConsumerSettingsBody,
    dead_letter_queue: Option<String>,
}

async fn upsert(state: &State, account: &AccountId, q: &QueueInfo, body: ConsumerBody) -> ApiResult<Value> {
    if body.ty.as_deref().is_some_and(|t| t != "worker") {
        return Err(ApiError::bad_request(
            "only Worker consumers are supported; HTTP pull consumers are not",
        ));
    }
    let script = body
        .script_name
        .or(body.script)
        .ok_or_else(|| ApiError::bad_request("a consumer needs `script_name`"))?;
    if state.db.script(account, &script).await?.is_none() {
        return Err(ApiError::bad_request(format!("Worker `{script}` does not exist")));
    }
    if let Some(dlq) = &body.dead_letter_queue {
        if state.db.queue(account, dlq).await?.is_none() {
            return Err(ApiError::bad_request(format!("dead letter queue `{dlq}` does not exist")));
        }
    }
    let s = &body.settings;
    let settings = ConsumerSettings {
        script,
        batch_size: s.batch_size.unwrap_or(10).clamp(1, 100),
        max_wait_ms: s.max_wait_time_ms.unwrap_or(5000).clamp(0, 60_000),
        max_retries: s.max_retries.unwrap_or(3).clamp(0, 100),
        retry_delay_secs: s.retry_delay.unwrap_or(0).clamp(0, 43_200),
        dead_letter_queue: body.dead_letter_queue,
    };
    state.db.set_consumer(account, &q.id, &settings).await?;
    Ok(consumer_json(q, &settings))
}

async fn add_consumer(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
    JsonBody(body): JsonBody<ConsumerBody>,
) -> ApiResult<Response> {
    let (id, q) = queue_access(&state, &caller, &account, &queue).await?;
    if let Some(existing) = state.db.consumer(&q.id).await? {
        let named = body.script_name.as_ref().or(body.script.as_ref());
        if named.is_some_and(|s| *s != existing.script) {
            return Err(ApiError::new(
                http::StatusCode::CONFLICT,
                11004,
                format!("Queue `{}` already has a consumer (`{}`)", q.name, existing.script),
            ));
        }
    }
    Ok(ok(upsert(&state, &id, &q, body).await?))
}

async fn update_consumer(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue, _consumer)): Path<(String, String, String)>,
    JsonBody(body): JsonBody<ConsumerBody>,
) -> ApiResult<Response> {
    let (id, q) = queue_access(&state, &caller, &account, &queue).await?;
    Ok(ok(upsert(&state, &id, &q, body).await?))
}

async fn delete_consumer(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue, _consumer)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let (_, q) = queue_access(&state, &caller, &account, &queue).await?;
    state.db.delete_consumer(&q.id).await?;
    Ok(ok(Value::Null))
}
