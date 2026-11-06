//! Queues: the producer side of workerd's queue binding protocol, and
//! delivery of batches to consumers.
//!
//! Producers (workerd's `Queue` binding) call the binding's service:
//! `POST /message` with the body and `X-Msg-Fmt` / `X-Msg-Delay-Secs`;
//! `POST /batch` with `{"messages": [{"body": base64, "contentType", "delaySecs"}]}`;
//! `GET /metrics`. Every response carries the queue's backlog.
//!
//! Bodies in the `json`, `text` and `bytes` formats are stored as sent (JSON
//! is the default format). The `v8` format (structured clone) is refused:
//! decoding it for the consumer would need workerd's experimental
//! `service_binding_extra_handlers` flag.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use selvedge_core::AccountId;
use selvedge_store::Db;
use selvedge_store::queues::{Consumer, Message, NewMessage, Outcome};
use serde_json::{Value, json};

use crate::agent::{ACCOUNT_HEADER, SCRIPT_HEADER, TOKEN_HEADER};

pub const MAX_MESSAGE_BYTES: usize = 128 * 1024;
pub const MAX_BATCH_MESSAGES: usize = 100;
pub const MAX_DELAY_SECS: u32 = 12 * 3600;
const FORMATS: &[&str] = &["json", "text", "bytes"];

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

fn format_of(raw: Option<&str>) -> Result<String, Response> {
    let f = raw.unwrap_or("json").to_ascii_lowercase();
    if f == "v8" {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Selvedge does not support the `v8` queue message format yet: send with contentType `json`, `text` or `bytes`",
        ));
    }
    if !FORMATS.contains(&f.as_str()) {
        return Err(error(StatusCode::BAD_REQUEST, format!("Unsupported queue message content type: {f}")));
    }
    Ok(f)
}

fn delay(raw: Option<u64>) -> Result<u32, Response> {
    let d = raw.unwrap_or(0);
    if d > u64::from(MAX_DELAY_SECS) {
        return Err(error(StatusCode::BAD_REQUEST, format!("delaySeconds must be at most {MAX_DELAY_SECS}")));
    }
    Ok(d as u32)
}

async fn backlog_response(db: &Db, queue_id: &str) -> Response {
    match db.backlog(queue_id).await {
        Ok((count, bytes)) => axum::Json(json!({
            "metadata": { "metrics": { "backlogCount": count, "backlogBytes": bytes } },
            "backlogCount": count,
            "backlogBytes": bytes,
        }))
        .into_response(),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "reading a queue backlog failed");
            error(StatusCode::SERVICE_UNAVAILABLE, "queue unavailable")
        }
    }
}

/// Handles a producer request for `queue` of `account`.
pub async fn produce(
    db: &Db,
    account: &AccountId,
    queue: &str,
    method: Method,
    path: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let q = match db.queue(account, queue).await {
        Ok(Some(q)) => q,
        Ok(None) => return error(StatusCode::NOT_FOUND, format!("Queue {queue} does not exist")),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "looking up a queue failed");
            return error(StatusCode::SERVICE_UNAVAILABLE, "queue unavailable");
        }
    };
    let messages = match (method, path) {
        (Method::GET, "/metrics") => return backlog_response(db, &q.id).await,
        (Method::POST, "/message") => {
            let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
            let content_type = match format_of(header("x-msg-fmt")) {
                Ok(f) => f,
                Err(r) => return r,
            };
            let delay_secs = match delay(header("x-msg-delay-secs").and_then(|d| d.parse().ok())) {
                Ok(d) => d,
                Err(r) => return r,
            };
            vec![NewMessage { body: body.to_vec(), content_type, delay_secs }]
        }
        (Method::POST, "/batch") => {
            let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                return error(StatusCode::BAD_REQUEST, "invalid batch");
            };
            let list = v["messages"].as_array().cloned().unwrap_or_default();
            if list.len() > MAX_BATCH_MESSAGES {
                return error(
                    StatusCode::BAD_REQUEST,
                    format!("a batch holds at most {MAX_BATCH_MESSAGES} messages"),
                );
            }
            let mut out = Vec::with_capacity(list.len());
            for m in list {
                let Some(bytes) = m["body"].as_str().and_then(|b| B64.decode(b).ok()) else {
                    return error(StatusCode::BAD_REQUEST, "invalid message body");
                };
                let content_type = match format_of(m["contentType"].as_str()) {
                    Ok(f) => f,
                    Err(r) => return r,
                };
                let delay_secs = match delay(m["delaySecs"].as_u64()) {
                    Ok(d) => d,
                    Err(r) => return r,
                };
                out.push(NewMessage { body: bytes, content_type, delay_secs });
            }
            out
        }
        _ => return error(StatusCode::NOT_FOUND, "not found"),
    };
    if let Some(m) = messages.iter().find(|m| m.body.len() > MAX_MESSAGE_BYTES) {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("message of {} bytes exceeds the 128 KiB limit", m.body.len()),
        );
    }
    if let Err(e) = db.enqueue(&q.id, &messages).await {
        tracing::error!(error = %format!("{e:#}"), "enqueueing failed");
        return error(StatusCode::SERVICE_UNAVAILABLE, "queue unavailable");
    }
    backlog_response(db, &q.id).await
}

/// Delivers batches to consumers. Runs in every data service replica; batches
/// are leased in the database, so replicas never deliver the same message at
/// once, and an unsettled lease (a crashed replica) expires and redelivers.
pub struct QueueDispatcher {
    pub db: Db,
    pub gateway_url: String,
    pub internal_token: String,
    pub http: reqwest::Client,
    busy: Mutex<HashSet<String>>,
}

const LEASE: Duration = Duration::from_secs(15 * 60);

impl QueueDispatcher {
    pub fn new(db: Db, gateway_url: String, internal_token: String) -> Arc<Self> {
        Arc::new(Self {
            db,
            gateway_url: gateway_url.trim_end_matches('/').to_owned(),
            internal_token,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(LEASE - Duration::from_secs(60))
                .build()
                .expect("HTTP client"),
            busy: Mutex::new(HashSet::new()),
        })
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let consumers = match self.db.ready_consumers().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "listing queue consumers failed");
                    continue;
                }
            };
            for c in consumers {
                if !self.busy.lock().expect("poisoned").insert(c.queue_id.clone()) {
                    continue;
                }
                let me = self.clone();
                tokio::spawn(async move {
                    let id = c.queue_id.clone();
                    if let Err(e) = me.deliver(&c).await {
                        tracing::warn!(queue = %c.queue_name, error = %format!("{e:#}"), "queue delivery failed");
                    }
                    me.busy.lock().expect("poisoned").remove(&id);
                });
            }
        }
    }

    async fn deliver(&self, c: &Consumer) -> anyhow::Result<()> {
        let batch =
            self.db.lease_batch(&c.queue_id, i64::from(c.settings.batch_size.clamp(1, 100)), LEASE).await?;
        if batch.is_empty() {
            return Ok(());
        }
        let messages: Vec<Value> = batch
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "timestamp": m.timestamp_ms,
                    "attempts": m.attempts,
                    "contentType": m.content_type,
                    "body": B64.encode(&m.body),
                })
            })
            .collect();
        let resp = self
            .http
            .post(format!("{}/queue", self.gateway_url))
            .header(TOKEN_HEADER, &self.internal_token)
            .header(ACCOUNT_HEADER, c.account.as_str())
            .header(SCRIPT_HEADER, selvedge_workerd::render::EVENTS_SERVICE)
            .json(&json!({ "script": c.settings.script, "queue": c.queue_name, "messages": messages }))
            .send()
            .await;
        let result: Value = match resp {
            Ok(r) if r.status().is_success() => {
                r.json().await.unwrap_or_else(|_| json!({ "outcome": "exception" }))
            }
            Ok(r) => json!({ "outcome": "exception", "error": format!("status {}", r.status()) }),
            Err(e) => json!({ "outcome": "exception", "error": e.to_string() }),
        };
        let outcomes = settle_plan(&batch, &result, c.settings.retry_delay_secs.max(0) as u32);
        let acked = outcomes.iter().filter(|(_, o)| matches!(o, Outcome::Ack)).count();
        tracing::info!(queue = %c.queue_name, script = %c.settings.script, messages = batch.len(), acked,
            outcome = %result["outcome"].as_str().unwrap_or("?"), "queue batch delivered");
        self.db.settle(&c.account, &c.settings, &outcomes).await
    }
}

/// Decides each message's fate from the consumer's result, as Cloudflare does:
/// explicit `ack()`/`retry()` win; then `ackAll()`/`retryAll()`; otherwise a
/// handler that returned acks everything and one that threw retries everything.
pub fn settle_plan(batch: &[Message], result: &Value, default_delay: u32) -> Vec<(Message, Outcome)> {
    let ids = |k: &str| -> HashSet<String> {
        result[k].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect()
    };
    let acks = ids("acks");
    let retries: std::collections::HashMap<String, Option<u32>> = result["retries"]
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_u64().map(|d| d as u32))).collect())
        .unwrap_or_default();
    let ok = result["outcome"].as_str() == Some("ok");
    let ack_all = result["ackAll"].as_bool().unwrap_or(false);
    let retry_all = result.get("retryAll").filter(|v| !v.is_null());
    batch
        .iter()
        .map(|m| {
            let outcome = if acks.contains(&m.id) {
                Outcome::Ack
            } else if let Some(d) = retries.get(&m.id) {
                Outcome::Retry { delay_secs: d.unwrap_or(default_delay) }
            } else if let Some(r) = retry_all {
                Outcome::Retry { delay_secs: r.as_u64().map_or(default_delay, |d| d as u32) }
            } else if ack_all || ok {
                Outcome::Ack
            } else {
                Outcome::Retry { delay_secs: default_delay }
            };
            (m.clone(), outcome)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str) -> Message {
        Message { id: id.into(), body: vec![], content_type: "json".into(), attempts: 1, timestamp_ms: 0 }
    }

    fn kinds(plan: &[(Message, Outcome)]) -> Vec<String> {
        plan.iter()
            .map(|(m, o)| match o {
                Outcome::Ack => format!("{}:ack", m.id),
                Outcome::Retry { delay_secs } => format!("{}:retry{delay_secs}", m.id),
            })
            .collect()
    }

    #[test]
    fn settling() {
        let b = [msg("a"), msg("b"), msg("c")];
        assert_eq!(kinds(&settle_plan(&b, &json!({"outcome": "ok"}), 0)), ["a:ack", "b:ack", "c:ack"]);
        assert_eq!(
            kinds(&settle_plan(&b, &json!({"outcome": "exception"}), 5)),
            ["a:retry5", "b:retry5", "c:retry5"]
        );
        let mixed = json!({"outcome": "exception", "acks": ["a"], "retries": {"b": 30}});
        assert_eq!(kinds(&settle_plan(&b, &mixed, 0)), ["a:ack", "b:retry30", "c:retry0"]);
        let retry_all = json!({"outcome": "ok", "retryAll": 7});
        assert_eq!(kinds(&settle_plan(&b, &retry_all, 0)), ["a:retry7", "b:retry7", "c:retry7"]);
    }

    #[test]
    fn formats() {
        assert_eq!(format_of(None).unwrap(), "json");
        assert_eq!(format_of(Some("TEXT")).unwrap(), "text");
        assert!(format_of(Some("v8")).is_err());
        assert!(format_of(Some("xml")).is_err());
    }
}
