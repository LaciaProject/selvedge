//! The KV REST API (`/storage/kv/namespaces`).

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use base64::Engine;
use bytes::Bytes;
use selvedge_core::{AccountId, NamespaceId};
use selvedge_store::KvEntry;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, State, account_access};
use crate::error::{ApiError, ApiResult, ok};
use crate::kvlogic;

pub fn routes() -> Router<State> {
    let ns = "/client/v4/accounts/{account}/storage/kv/namespaces";
    Router::new()
        .route(ns, get(list_namespaces).post(create_namespace))
        .route(&format!("{ns}/{{ns}}"), get(get_namespace).put(rename_namespace).delete(delete_namespace))
        .route(&format!("{ns}/{{ns}}/keys"), get(list_keys))
        .route(&format!("{ns}/{{ns}}/values/{{*key}}"), get(get_value).put(put_value).delete(delete_value))
        .route(&format!("{ns}/{{ns}}/metadata/{{*key}}"), get(get_metadata))
        .route(&format!("{ns}/{{ns}}/bulk"), put(bulk_put).delete(bulk_delete_legacy))
        .route(&format!("{ns}/{{ns}}/bulk/delete"), post(bulk_delete))
}

fn ns_json(id: &NamespaceId, title: &str) -> Value {
    json!({ "id": id, "title": title, "supports_url_encoding": true })
}

async fn namespace_access(
    state: &State,
    caller: &Caller,
    account: &str,
    ns: &str,
) -> ApiResult<(AccountId, NamespaceId)> {
    let account = account_access(state, caller, account).await?;
    let not_found = || ApiError::new(http::StatusCode::NOT_FOUND, 10013, "namespace not found");
    let ns: NamespaceId = ns.parse().map_err(|_| not_found())?;
    state.db.kv_namespace_owned(&account, &ns).await?.ok_or_else(not_found)?;
    Ok((account, ns))
}

#[derive(Deserialize)]
struct Paging {
    page: Option<usize>,
    per_page: Option<usize>,
}

async fn list_namespaces(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    Query(p): Query<Paging>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let all = state.db.kv_namespaces(&id).await?;
    let per_page = p.per_page.unwrap_or(20).clamp(5, 100);
    let page = p.page.unwrap_or(1).max(1);
    let items: Vec<_> =
        all.iter().skip((page - 1) * per_page).take(per_page).map(|n| ns_json(&n.id, &n.title)).collect();
    let info = json!({ "page": page, "per_page": per_page, "count": items.len(), "total_count": all.len() });
    Ok(Json(selvedge_core::envelope::Envelope::ok_paged(json!(items), info)).into_response())
}

#[derive(Deserialize)]
struct TitleBody {
    title: String,
}

async fn create_namespace(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    Json(body): Json<TitleBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    if body.title.is_empty() || body.title.len() > 512 {
        return Err(ApiError::bad_request("title must be 1-512 characters"));
    }
    let ns = state.db.create_kv_namespace(&id, &body.title).await?.ok_or_else(|| {
        ApiError::new(
            http::StatusCode::BAD_REQUEST,
            10014,
            "A namespace with this account ID and title already exists",
        )
    })?;
    Ok(ok(ns_json(&ns.id, &ns.title)))
}

async fn get_namespace(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (account, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    let n = state
        .db
        .kv_namespace_owned(&account, &ns)
        .await?
        .ok_or_else(|| ApiError::not_found("namespace not found"))?;
    Ok(ok(ns_json(&n.id, &n.title)))
}

async fn rename_namespace(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns)): Path<(String, String)>,
    Json(body): Json<TitleBody>,
) -> ApiResult<Response> {
    let (account, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    state.db.rename_kv_namespace(&account, &ns, &body.title).await?;
    Ok(ok(Value::Null))
}

async fn delete_namespace(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (account, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    state.db.delete_kv_namespace(&account, &ns).await?;
    Ok(ok(Value::Null))
}

async fn list_keys(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let (_, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    let limit = kvlogic::list_limit(q.get("limit").map(String::as_str)).map_err(ApiError::bad_request)?;
    let after = q
        .get("cursor")
        .filter(|c| !c.is_empty())
        .map(|c| kvlogic::decode_cursor(c))
        .transpose()
        .map_err(ApiError::bad_request)?;
    let prefix = q.get("prefix").map(String::as_str).unwrap_or("");
    let (keys, more) = state.db.kv_list(&ns, prefix, after.as_deref(), limit).await?;
    let cursor = if more {
        keys.last().map(|k| kvlogic::encode_cursor(&k.name)).unwrap_or_default()
    } else {
        String::new()
    };
    let info = json!({ "count": keys.len(), "cursor": cursor });
    Ok(Json(selvedge_core::envelope::Envelope::ok_paged(json!(keys), info)).into_response())
}

async fn get_value(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns, key)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let (_, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    let entry = state
        .db
        .kv_get(&ns, &key)
        .await?
        .ok_or_else(|| ApiError::new(http::StatusCode::NOT_FOUND, 10009, "get: 'key not found'"))?;
    let mut resp = Response::new(axum::body::Body::from(entry.value));
    if let Some(exp) = entry.expiration {
        if let Ok(v) = exp.to_string().parse() {
            resp.headers_mut().insert("expiration", v);
        }
    }
    Ok(resp)
}

async fn get_metadata(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns, key)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let (_, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    let entry = state
        .db
        .kv_get(&ns, &key)
        .await?
        .ok_or_else(|| ApiError::new(http::StatusCode::NOT_FOUND, 10009, "metadata: 'key not found'"))?;
    let meta = entry.metadata.and_then(|m| serde_json::from_str::<Value>(&m).ok()).unwrap_or(Value::Null);
    Ok(ok(meta))
}

async fn put_value(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns, key)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let (_, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    kvlogic::validate_key(&key).map_err(ApiError::bad_request)?;
    let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
    let (value, metadata) = if ct.starts_with("multipart/form-data") {
        parse_value_form(ct, body).await?
    } else {
        (body.to_vec(), None)
    };
    if value.len() > kvlogic::MAX_VALUE_BYTES {
        return Err(ApiError::new(http::StatusCode::PAYLOAD_TOO_LARGE, 10011, "Value length exceeds limit"));
    }
    kvlogic::validate_metadata(metadata.as_deref()).map_err(ApiError::bad_request)?;
    let expiration = kvlogic::expiration(
        q.get("expiration").map(String::as_str),
        q.get("expiration_ttl").map(String::as_str),
    )
    .map_err(ApiError::bad_request)?;
    state.db.kv_put(&ns, &key, &KvEntry { value, metadata, expiration }).await?;
    Ok(ok(Value::Null))
}

/// `value` and `metadata` parts of a multipart value upload.
async fn parse_value_form(ct: &str, body: Bytes) -> ApiResult<(Vec<u8>, Option<String>)> {
    let boundary = ct
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_owned())
        .ok_or_else(|| ApiError::bad_request("multipart body without boundary"))?;
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(body) });
    let mut form = multer::Multipart::new(stream, boundary);
    let (mut value, mut metadata) = (None, None);
    while let Some(field) = form.next_field().await.map_err(|e| ApiError::bad_request(e.to_string()))? {
        let name = field.name().unwrap_or_default().to_owned();
        let bytes = field.bytes().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
        match name.as_str() {
            "value" => value = Some(bytes.to_vec()),
            "metadata" => {
                metadata = Some(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|_| ApiError::bad_request("metadata must be UTF-8"))?,
                )
            }
            _ => {}
        }
    }
    Ok((value.ok_or_else(|| ApiError::bad_request("missing `value` part"))?, metadata))
}

async fn delete_value(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns, key)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let (_, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    state.db.kv_delete(&ns, &[key]).await?;
    Ok(ok(Value::Null))
}

#[derive(Deserialize)]
struct BulkItem {
    key: String,
    value: String,
    #[serde(default)]
    base64: bool,
    expiration: Option<i64>,
    expiration_ttl: Option<i64>,
    metadata: Option<Value>,
}

async fn bulk_put(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns)): Path<(String, String)>,
    Json(items): Json<Vec<BulkItem>>,
) -> ApiResult<Response> {
    let (_, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    if items.len() > 10_000 {
        return Err(ApiError::bad_request("at most 10,000 pairs per request"));
    }
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        kvlogic::validate_key(&item.key).map_err(ApiError::bad_request)?;
        let value = if item.base64 {
            base64::engine::general_purpose::STANDARD
                .decode(&item.value)
                .map_err(|_| ApiError::bad_request(format!("value of `{}` is not valid base64", item.key)))?
        } else {
            item.value.into_bytes()
        };
        let metadata = item.metadata.map(|m| m.to_string());
        kvlogic::validate_metadata(metadata.as_deref()).map_err(ApiError::bad_request)?;
        let expiration = kvlogic::expiration(
            item.expiration.map(|e| e.to_string()).as_deref(),
            item.expiration_ttl.map(|e| e.to_string()).as_deref(),
        )
        .map_err(ApiError::bad_request)?;
        entries.push((item.key, KvEntry { value, metadata, expiration }));
    }
    for (key, entry) in &entries {
        state.db.kv_put(&ns, key, entry).await?;
    }
    Ok(ok(json!({ "successful_key_count": entries.len(), "unsuccessful_keys": [] })))
}

async fn bulk_delete(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, ns)): Path<(String, String)>,
    Json(keys): Json<Vec<String>>,
) -> ApiResult<Response> {
    let (_, ns) = namespace_access(&state, &caller, &account, &ns).await?;
    if keys.len() > 10_000 {
        return Err(ApiError::bad_request("at most 10,000 keys per request"));
    }
    state.db.kv_delete(&ns, &keys).await?;
    Ok(ok(json!({ "successful_key_count": keys.len(), "unsuccessful_keys": [] })))
}

async fn bulk_delete_legacy(
    state: AxState<State>,
    caller: Caller,
    path: Path<(String, String)>,
    keys: Json<Vec<String>>,
) -> ApiResult<Response> {
    bulk_delete(state, caller, path, keys).await
}
