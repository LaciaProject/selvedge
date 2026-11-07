//! The Queues REST API: queues, their (one) consumer, a Worker or an HTTP
//! pull consumer, and pushing, pulling and acking messages over HTTP.

use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::response::Response;
use axum::routing::{get, post, put};
use base64::Engine as _;
use selvedge_core::AccountId;
use selvedge_store::queues::{ConsumerSettings, QueueInfo, QueueUpdate};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, JsonBody, State, account_access};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/queues";
    Router::new()
        .route(base, get(list).post(create))
        .route(
            &format!("{base}/{{queue}}"),
            get(get_queue).delete(delete_queue).patch(update_queue).put(update_queue),
        )
        .route(&format!("{base}/{{queue}}/purge"), post(purge))
        .route(&format!("{base}/{{queue}}/consumers"), get(list_consumers).post(add_consumer))
        .route(
            &format!("{base}/{{queue}}/consumers/{{consumer}}"),
            put(update_consumer).delete(delete_consumer).get(get_consumer),
        )
        .route(&format!("{base}/{{queue}}/messages"), post(push))
        .route(&format!("{base}/{{queue}}/messages/batch"), post(push_batch))
        .route(&format!("{base}/{{queue}}/messages/pull"), post(pull))
        .route(&format!("{base}/{{queue}}/messages/ack"), post(ack))
}

/// Largest message, as on Cloudflare.
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
const MAX_BATCH: usize = 100;
const MAX_DELAY_SECS: u32 = 43_200;

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
    if c.is_http_pull() {
        return json!({
            "consumer_id": q.id,
            "queue_id": q.id,
            "queue_name": q.name,
            "type": "http_pull",
            "created_on": q.created_on,
            "settings": {
                "batch_size": c.batch_size,
                "max_retries": c.max_retries,
                "visibility_timeout_ms": c.visibility_timeout_ms,
                "retry_delay": c.retry_delay_secs,
            },
            "dead_letter_queue": c.dead_letter_queue,
        });
    }
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
        "settings": {
            "delivery_delay": q.delivery_delay_secs,
            "delivery_paused": q.delivery_paused,
            "message_retention_period": q.retention_secs,
        },
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
    visibility_timeout_ms: Option<i32>,
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
    let kind = body.ty.clone().unwrap_or_else(|| "worker".into());
    let script = match kind.as_str() {
        "worker" => {
            let script = body
                .script_name
                .or(body.script)
                .ok_or_else(|| ApiError::bad_request("a consumer needs `script_name`"))?;
            if state.db.script(account, &script).await?.is_none() {
                return Err(ApiError::bad_request(format!("Worker `{script}` does not exist")));
            }
            script
        }
        "http_pull" => String::new(),
        other => {
            return Err(ApiError::bad_request(format!(
                "consumer type `{other}` is not supported; use `worker` or `http_pull`"
            )));
        }
    };
    if let Some(dlq) = &body.dead_letter_queue {
        if state.db.queue(account, dlq).await?.is_none() {
            return Err(ApiError::bad_request(format!("dead letter queue `{dlq}` does not exist")));
        }
    }
    let s = &body.settings;
    let settings = ConsumerSettings {
        kind,
        visibility_timeout_ms: s.visibility_timeout_ms.unwrap_or(30_000).clamp(1, 12 * 3600 * 1000),
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
        let other_kind = body.ty.as_deref().unwrap_or("worker") != existing.kind;
        if other_kind || named.is_some_and(|s| *s != existing.script) {
            return Err(ApiError::new(
                http::StatusCode::CONFLICT,
                11004,
                format!(
                    "Queue `{}` already has a consumer (`{}`)",
                    q.name,
                    if existing.is_http_pull() { "http_pull" } else { &existing.script }
                ),
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

// ---- messages over HTTP ----

#[derive(Deserialize)]
struct PushMessage {
    body: Value,
    content_type: Option<String>,
    delay_seconds: Option<u32>,
}

/// A pushed message, encoded as the binding encodes it: `json` holds JSON
/// text, `text` the string.
fn new_message(m: PushMessage, batch_delay: Option<u32>) -> ApiResult<selvedge_store::queues::NewMessage> {
    let content_type = m.content_type.unwrap_or_else(|| "json".into());
    let body = match content_type.as_str() {
        "json" => m.body.to_string().into_bytes(),
        "text" => match m.body {
            Value::String(s) => s.into_bytes(),
            _ => return Err(ApiError::bad_request("a `text` message needs a string body")),
        },
        other => {
            return Err(ApiError::bad_request(format!(
                "content_type `{other}` is not supported; use `json` or `text`"
            )));
        }
    };
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(ApiError::bad_request("a message may be at most 128 KB"));
    }
    let delay_secs = m.delay_seconds.or(batch_delay);
    if delay_secs.is_some_and(|d| d > MAX_DELAY_SECS) {
        return Err(ApiError::bad_request("delay_seconds may be at most 43200"));
    }
    Ok(selvedge_store::queues::NewMessage { body, content_type, delay_secs })
}

async fn push(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
    JsonBody(body): JsonBody<PushMessage>,
) -> ApiResult<Response> {
    let (_, q) = queue_access(&state, &caller, &account, &queue).await?;
    state.db.enqueue(&q.id, &[new_message(body, None)?]).await?;
    Ok(ok(Value::Null))
}

#[derive(Deserialize)]
struct PushBatch {
    messages: Vec<PushMessage>,
    delay_seconds: Option<u32>,
}

async fn push_batch(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
    JsonBody(body): JsonBody<PushBatch>,
) -> ApiResult<Response> {
    let (_, q) = queue_access(&state, &caller, &account, &queue).await?;
    if body.messages.len() > MAX_BATCH {
        return Err(ApiError::bad_request("a batch may hold at most 100 messages"));
    }
    let messages = body
        .messages
        .into_iter()
        .map(|m| new_message(m, body.delay_seconds))
        .collect::<ApiResult<Vec<_>>>()?;
    state.db.enqueue(&q.id, &messages).await?;
    Ok(ok(Value::Null))
}

/// The queue's HTTP pull consumer, or 400.
async fn pull_consumer(state: &State, q: &QueueInfo) -> ApiResult<ConsumerSettings> {
    state.db.consumer(&q.id).await?.filter(ConsumerSettings::is_http_pull).ok_or_else(|| {
        ApiError::bad_request(format!(
            "Queue `{}` has no HTTP pull consumer; add one with `wrangler queues consumer http add`",
            q.name
        ))
    })
}

#[derive(Deserialize, Default)]
struct PullBody {
    batch_size: Option<i64>,
    visibility_timeout_ms: Option<i64>,
}

async fn pull(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let (id, q) = queue_access(&state, &caller, &account, &queue).await?;
    let c = pull_consumer(&state, &q).await?;
    let req: PullBody = if body.is_empty() {
        PullBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    if q.delivery_paused {
        let (backlog, _) = state.db.backlog(&q.id).await?;
        return Ok(ok(json!({ "messages": [], "message_backlog_count": backlog })));
    }
    let limit = req.batch_size.unwrap_or(i64::from(c.batch_size)).clamp(1, MAX_BATCH as i64);
    let visibility =
        req.visibility_timeout_ms.unwrap_or(i64::from(c.visibility_timeout_ms)).clamp(1, 12 * 3600 * 1000);
    state.db.expire_pulled(&id, &q.id, &c).await?;
    let messages =
        state.db.lease_batch(&q.id, limit, std::time::Duration::from_millis(visibility as u64)).await?;
    let (backlog, _) = state.db.backlog(&q.id).await?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let out: Vec<Value> = messages
        .iter()
        .map(|m| {
            let body = match m.content_type.as_str() {
                "bytes" => b64.encode(&m.body),
                _ => String::from_utf8_lossy(&m.body).into_owned(),
            };
            json!({
                "id": m.id,
                "body": body,
                "timestamp_ms": m.timestamp_ms,
                "attempts": m.attempts,
                "lease_id": selvedge_store::queues::lease_id(m),
                "metadata": { "CF-sourceMessageSource": "api", "CF-Content-Type": m.content_type },
            })
        })
        .collect();
    Ok(ok(json!({ "messages": out, "message_backlog_count": backlog })))
}

#[derive(Deserialize)]
struct LeaseRef {
    lease_id: String,
    delay_seconds: Option<u32>,
}

#[derive(Deserialize)]
struct AckBody {
    #[serde(default)]
    acks: Vec<LeaseRef>,
    #[serde(default)]
    retries: Vec<LeaseRef>,
}

async fn ack(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
    JsonBody(body): JsonBody<AckBody>,
) -> ApiResult<Response> {
    let (id, q) = queue_access(&state, &caller, &account, &queue).await?;
    let c = pull_consumer(&state, &q).await?;
    let acks: Vec<String> = body.acks.into_iter().map(|a| a.lease_id).collect();
    let retries: Vec<(String, u32)> = body
        .retries
        .into_iter()
        .map(|r| {
            (r.lease_id, r.delay_seconds.unwrap_or(c.retry_delay_secs.max(0) as u32).min(MAX_DELAY_SECS))
        })
        .collect();
    let (acked, retried) = state.db.settle_pulled(&id, &q.id, &c, &acks, &retries).await?;
    let stale = (acks.len() + retries.len()) as u64 - acked - retried;
    let warnings: Vec<String> = if stale > 0 {
        vec![format!("{stale} lease(s) had expired or were unknown and were skipped")]
    } else {
        vec![]
    };
    Ok(ok(json!({ "ackCount": acked, "retryCount": retried, "warnings": warnings })))
}

// ---- queue settings ----

#[derive(Deserialize, Default)]
struct SettingsBody {
    delivery_paused: Option<bool>,
    delivery_delay: Option<i32>,
    message_retention_period: Option<i32>,
}

#[derive(Deserialize)]
struct UpdateBody {
    queue_name: Option<String>,
    #[serde(default)]
    settings: SettingsBody,
}

/// `wrangler queues update`, `pause-delivery` and `resume-delivery`.
async fn update_queue(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
    JsonBody(body): JsonBody<UpdateBody>,
) -> ApiResult<Response> {
    let (id, q) = queue_access(&state, &caller, &account, &queue).await?;
    let s = body.settings;
    if s.delivery_delay.is_some_and(|d| !(0..=MAX_DELAY_SECS as i32).contains(&d)) {
        return Err(ApiError::bad_request("delivery_delay must be between 0 and 43200 seconds"));
    }
    // Cloudflare allows 60 seconds to 14 days.
    if s.message_retention_period.is_some_and(|r| !(60..=14 * 86_400).contains(&r)) {
        return Err(ApiError::bad_request("message_retention_period must be between 60 and 1209600 seconds"));
    }
    let name = body.queue_name.filter(|n| *n != q.name);
    if let Some(n) = &name {
        if !valid_name(n) {
            return Err(ApiError::bad_request(format!("`{n}` is not a valid queue name")));
        }
        if state.db.queue(&id, n).await?.is_some() {
            return Err(ApiError::new(
                http::StatusCode::CONFLICT,
                11009,
                format!("Queue `{n}` already exists"),
            ));
        }
    }
    let update = QueueUpdate {
        name,
        delivery_paused: s.delivery_paused,
        delivery_delay_secs: s.delivery_delay,
        retention_secs: s.message_retention_period,
    };
    let q = state.db.update_queue(&id, &q.id, &update).await?.ok_or_else(not_found)?;
    Ok(ok(queue_json(&state, &q).await?))
}

/// `wrangler queues purge`: deletes every message.
async fn purge(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, queue)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (_, q) = queue_access(&state, &caller, &account, &queue).await?;
    state.db.purge_queue(&q.id).await?;
    Ok(ok(json!({ "started_on": selvedge_store::repo::rfc3339(std::time::SystemTime::now()) })))
}
