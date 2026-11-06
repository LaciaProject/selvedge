//! The binding data service: the server side of workerd's KV binding protocol.
//!
//! workerd sends KV calls to the binding's service as HTTP:
//!
//! - `GET /<key>?urlencoded=true[&cache_ttl=n]`: the value, with JSON metadata in
//!   `CF-KV-Metadata`; 404 when missing.
//! - `POST /bulk/get` with `{"keys": [...], "type"?, "withMetadata"?}`.
//! - `GET /?[prefix=][&key_count_limit=][&cursor=]`: a list.
//! - `PUT /<key>?urlencoded=true[&expiration=|&expiration_ttl=]`, metadata in
//!   `CF-KV-Metadata`.
//! - `DELETE /<key>?urlencoded=true`.
//!
//! Requests arrive from agents only. The agent sets `x-selvedge-account` from the
//! Unix socket the request came in on; workerd's config injects
//! `x-selvedge-kv-namespace`. The pair is checked against the database, so a
//! compromised agent still cannot reach a namespace of an account it does not
//! claim.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State as AxState};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use selvedge_core::{AccountId, NamespaceId};
use selvedge_store::{Blobs, Db, KvEntry};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::kvlogic;

pub const ACCOUNT_HEADER: &str = "x-selvedge-account";

pub struct DataState {
    pub db: Db,
    pub blobs: Blobs,
    pub internal_token: String,
    /// (account, namespace) pairs verified recently.
    owned: Mutex<HashMap<(AccountId, NamespaceId), Instant>>,
    /// (account, bucket name) to bucket id, resolved recently.
    buckets: Mutex<HashMap<(AccountId, String), (String, Instant)>>,
}

impl DataState {
    pub fn new(db: Db, blobs: Blobs, internal_token: String) -> Arc<Self> {
        Arc::new(Self {
            db,
            blobs,
            internal_token,
            owned: Mutex::new(HashMap::new()),
            buckets: Mutex::new(HashMap::new()),
        })
    }
}

const OWNERSHIP_TTL: Duration = Duration::from_secs(60);

pub fn router(state: Arc<DataState>) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .fallback(dispatch)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state)
}

fn token_ok(state: &DataState, headers: &HeaderMap) -> bool {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| crate::constant_time_eq(t.as_bytes(), state.internal_token.as_bytes()))
}

/// Routes a binding call: R2 calls stream; KV calls are small and buffered.
async fn dispatch(AxState(state): AxState<Arc<DataState>>, req: axum::extract::Request) -> Response {
    if !token_ok(&state, req.headers()) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if req.headers().contains_key(crate::r2::BUCKET_HEADER) {
        return r2_dispatch(&state, req).await;
    }
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, kvlogic::MAX_VALUE_BYTES + 4096).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "Value too large");
    };
    let q = Query::<HashMap<String, String>>::try_from_uri(&parts.uri).map(|q| q.0).unwrap_or_default();
    handle(&state, parts.method, parts.uri, q, parts.headers, body).await
}

const BUCKET_TTL: Duration = Duration::from_secs(60);

async fn r2_dispatch(state: &DataState, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let (Some(account), Some(name)) =
        (header(ACCOUNT_HEADER).and_then(|v| v.parse::<AccountId>().ok()), header(crate::r2::BUCKET_HEADER))
    else {
        return error(StatusCode::BAD_REQUEST, "missing account or bucket");
    };
    let key = (account.clone(), name.clone());
    let cached = state
        .buckets
        .lock()
        .expect("poisoned")
        .get(&key)
        .filter(|(_, t)| t.elapsed() < BUCKET_TTL)
        .map(|(id, _)| id.clone());
    let id = match cached {
        Some(id) => id,
        None => match state.db.r2_bucket(&account, &name).await {
            Ok(Some((id, _))) => {
                state.buckets.lock().expect("poisoned").insert(key, (id.clone(), Instant::now()));
                id
            }
            Ok(None) => {
                return crate::r2::R2Error::new(
                    StatusCode::NOT_FOUND,
                    10006,
                    "The specified bucket does not exist.",
                )
                .into_response();
            }
            Err(e) => return crate::r2::R2Error::from(e).into_response(),
        },
    };
    let bucket = crate::r2::Bucket { db: &state.db, blobs: &state.blobs, id };
    match parts.method {
        Method::GET => crate::r2::binding_get(&bucket, &parts.headers).await,
        Method::PUT => crate::r2::binding_put(&bucket, &parts.headers, body).await,
        _ => error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

async fn authorize(state: &DataState, headers: &HeaderMap) -> Result<NamespaceId, Response> {
    let token_ok = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| crate::constant_time_eq(t.as_bytes(), state.internal_token.as_bytes()));
    if !token_ok {
        return Err(error(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let account: AccountId = header(ACCOUNT_HEADER)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "missing account"))?;
    let ns: NamespaceId = header(selvedge_workerd::render::KV_NAMESPACE_HEADER)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "missing namespace"))?;
    let key = (account.clone(), ns.clone());
    let fresh = state.owned.lock().expect("poisoned").get(&key).is_some_and(|t| t.elapsed() < OWNERSHIP_TTL);
    if !fresh {
        match state.db.kv_namespace_owned(&account, &ns).await {
            Ok(Some(_)) => {
                state.owned.lock().expect("poisoned").insert(key, Instant::now());
            }
            Ok(None) => return Err(error(StatusCode::NOT_FOUND, "namespace not found")),
            Err(e) => {
                tracing::error!(error = %e, "checking namespace ownership");
                return Err(error(StatusCode::SERVICE_UNAVAILABLE, "storage unavailable"));
            }
        }
    }
    Ok(ns)
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!(error = %format!("{e:#}"), "KV storage error");
    error(StatusCode::SERVICE_UNAVAILABLE, "storage unavailable")
}

async fn handle(
    state: &DataState,
    method: Method,
    uri: Uri,
    q: HashMap<String, String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ns = match authorize(state, &headers).await {
        Ok(ns) => ns,
        Err(r) => return r,
    };
    let path = uri.path();
    if method == Method::POST && path == "/bulk/get" {
        return bulk_get(state, &ns, &body).await.unwrap_or_else(|r| r);
    }
    if method == Method::GET && path == "/" {
        return list(state, &ns, &q).await.unwrap_or_else(|r| r);
    }
    let raw = path.trim_start_matches('/');
    let key = match percent_decode_str(raw).decode_utf8() {
        Ok(k) => k.into_owned(),
        Err(_) => return error(StatusCode::BAD_REQUEST, "key is not valid UTF-8"),
    };
    if let Err(e) = kvlogic::validate_key(&key) {
        return error(StatusCode::BAD_REQUEST, e);
    }
    let result = match method {
        Method::GET => get(state, &ns, &key).await,
        Method::PUT => put(state, &ns, &key, &q, &headers, body).await,
        Method::DELETE => {
            state.db.kv_delete(&ns, &[key]).await.map(|_| StatusCode::OK.into_response()).map_err(internal)
        }
        _ => Err(error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")),
    };
    result.unwrap_or_else(|r| r)
}

async fn get(state: &DataState, ns: &NamespaceId, key: &str) -> Result<Response, Response> {
    let Some(entry) = state.db.kv_get(ns, key).await.map_err(internal)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let mut resp = Response::new(axum::body::Body::from(entry.value));
    if let Some(meta) = entry.metadata.and_then(|m| m.parse().ok()) {
        resp.headers_mut().insert("cf-kv-metadata", meta);
    }
    if let Some(exp) = entry.expiration.and_then(|e| e.to_string().parse().ok()) {
        resp.headers_mut().insert("expiration", exp);
    }
    Ok(resp)
}

async fn put(
    state: &DataState,
    ns: &NamespaceId,
    key: &str,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    if body.len() > kvlogic::MAX_VALUE_BYTES {
        return Err(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Value length of {} exceeds limit of {}.", body.len(), kvlogic::MAX_VALUE_BYTES),
        ));
    }
    let metadata = headers.get("cf-kv-metadata").and_then(|v| v.to_str().ok()).map(str::to_owned);
    kvlogic::validate_metadata(metadata.as_deref()).map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    let expiration = kvlogic::expiration(
        q.get("expiration").map(String::as_str),
        q.get("expiration_ttl").map(String::as_str),
    )
    .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    state
        .db
        .kv_put(ns, key, &KvEntry { value: body.to_vec(), metadata, expiration })
        .await
        .map_err(internal)?;
    Ok(StatusCode::OK.into_response())
}

async fn list(
    state: &DataState,
    ns: &NamespaceId,
    q: &HashMap<String, String>,
) -> Result<Response, Response> {
    let limit = kvlogic::list_limit(q.get("key_count_limit").map(String::as_str))
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    let after = q
        .get("cursor")
        .filter(|c| !c.is_empty())
        .map(|c| kvlogic::decode_cursor(c))
        .transpose()
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    let prefix = q.get("prefix").map(String::as_str).unwrap_or("");
    let (keys, more) = state.db.kv_list(ns, prefix, after.as_deref(), limit).await.map_err(internal)?;
    let mut out = json!({ "keys": keys, "list_complete": !more, "cacheStatus": null });
    if more {
        if let Some(last) = keys.last() {
            out["cursor"] = json!(kvlogic::encode_cursor(&last.name));
        }
    }
    Ok(axum::Json(out).into_response())
}

#[derive(Deserialize)]
struct BulkGet {
    keys: Vec<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
    #[serde(rename = "withMetadata", default)]
    with_metadata: bool,
}

async fn bulk_get(state: &DataState, ns: &NamespaceId, body: &[u8]) -> Result<Response, Response> {
    let req: BulkGet =
        serde_json::from_slice(body).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
    if req.keys.is_empty() || req.keys.len() > 100 {
        return Err(error(StatusCode::BAD_REQUEST, "You can request a maximum of 100 keys"));
    }
    let json_type = match req.ty.as_deref() {
        None | Some("text") => false,
        Some("json") => true,
        Some(other) => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                format!("\"{other}\" is not a valid type. Use \"json\" or \"text\""),
            ));
        }
    };
    for k in &req.keys {
        kvlogic::validate_key(k).map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    }
    let found: HashMap<String, KvEntry> =
        state.db.kv_get_many(ns, &req.keys).await.map_err(internal)?.into_iter().collect();
    let mut out = Map::new();
    for key in &req.keys {
        let value = match found.get(key) {
            None => Value::Null,
            Some(entry) => {
                let text = String::from_utf8_lossy(&entry.value).into_owned();
                let value = if json_type {
                    serde_json::from_str(&text).map_err(|_| {
                        error(StatusCode::BAD_REQUEST, format!("The value for \"{key}\" is not valid JSON"))
                    })?
                } else {
                    Value::String(text)
                };
                if req.with_metadata {
                    let metadata = entry
                        .metadata
                        .as_deref()
                        .and_then(|m| serde_json::from_str(m).ok())
                        .unwrap_or(Value::Null);
                    let mut o = json!({ "value": value, "metadata": metadata });
                    if let Some(exp) = entry.expiration {
                        o["expiration"] = json!(exp);
                    }
                    o
                } else {
                    value
                }
            }
        };
        out.insert(key.clone(), value);
    }
    Ok(axum::Json(Value::Object(out)).into_response())
}
