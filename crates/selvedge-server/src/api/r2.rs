//! The R2 REST API: buckets, and objects as `wrangler r2 object` uses them.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State as AxState};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use selvedge_core::AccountId;
use selvedge_store::r2::BucketDelete;
use selvedge_store::r2public::CustomDomain;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, State, account_access};
use crate::error::{ApiError, ApiResult, ok};
use crate::r2::{Bucket, GetResult, object_json, read_object};

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/r2/buckets";
    Router::new()
        .route(base, get(list).post(create))
        .route(&format!("{base}/{{bucket}}"), get(get_bucket).delete(delete_bucket))
        .route(
            &format!("{base}/{{bucket}}/objects/{{*key}}"),
            get(get_object).put(put_object).delete(delete_object),
        )
        .route(&format!("{base}/{{bucket}}/domains/managed"), get(get_dev_url).put(put_dev_url))
        .route(&format!("{base}/{{bucket}}/domains/custom"), get(list_custom).post(add_custom))
        .route(
            &format!("{base}/{{bucket}}/domains/custom/{{domain}}"),
            get(get_custom).put(update_custom).delete(delete_custom),
        )
}

pub fn valid_bucket_name(name: &str) -> bool {
    (3..=63).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(super) fn iso(t: std::time::SystemTime) -> String {
    let ms = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
    // Seconds precision is enough for clients; reuse the store's formatting.
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem / 60 % 60, rem % 60, ms % 1000)
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn bucket_json(name: &str, created: std::time::SystemTime) -> Value {
    json!({ "name": name, "creation_date": iso(created), "location": "auto", "storage_class": "Standard" })
}

fn no_bucket() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 10006, "The specified bucket does not exist.")
}

pub(super) async fn bucket_id(state: &State, account: &AccountId, name: &str) -> ApiResult<String> {
    Ok(state.db.r2_bucket(account, name).await?.ok_or_else(no_bucket)?.0)
}

fn r2_error(e: crate::r2::R2Error) -> ApiError {
    ApiError::new(e.status, e.code, e.message)
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let buckets = state.db.r2_buckets(&id).await?;
    Ok(ok(json!({ "buckets": buckets.iter().map(|(n, c)| bucket_json(n, *c)).collect::<Vec<_>>() })))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
}

async fn create(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    super::JsonBody(body): super::JsonBody<CreateBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    if !valid_bucket_name(&body.name) {
        return Err(ApiError::new(
            http::StatusCode::BAD_REQUEST,
            10005,
            "The specified bucket name is not valid: use 3-63 lowercase letters, digits and hyphens.",
        ));
    }
    if state.db.create_r2_bucket(&id, &body.name).await?.is_none() {
        return Err(ApiError::new(
            http::StatusCode::CONFLICT,
            10004,
            "The bucket you tried to create already exists.",
        ));
    }
    Ok(ok(bucket_json(&body.name, std::time::SystemTime::now())))
}

async fn get_bucket(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let (_, created) = state.db.r2_bucket(&id, &bucket).await?.ok_or_else(no_bucket)?;
    Ok(ok(bucket_json(&bucket, created)))
}

async fn delete_bucket(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    match state.db.delete_r2_bucket(&id, &bucket).await? {
        BucketDelete::Deleted => Ok(ok(Value::Null)),
        BucketDelete::NotFound => Err(no_bucket()),
        BucketDelete::NotEmpty => Err(ApiError::new(
            http::StatusCode::CONFLICT,
            10008,
            "The bucket you tried to delete is not empty.",
        )),
    }
}

const HTTP_HEADERS: &[(&str, &str)] = &[
    ("content-type", "contentType"),
    ("content-language", "contentLanguage"),
    ("content-disposition", "contentDisposition"),
    ("content-encoding", "contentEncoding"),
    ("cache-control", "cacheControl"),
];

async fn put_object(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let b = Bucket { db: &state.db, blobs: &state.blobs, id: bucket_id(&state, &id, &bucket).await? };
    let mut http = serde_json::Map::new();
    for (h, field) in HTTP_HEADERS {
        if let Some(v) = headers.get(*h).and_then(|v| v.to_str().ok()) {
            http.insert((*field).into(), json!(v));
        }
    }
    let req = json!({ "httpFields": http });
    let o =
        b.put(&key, &req, body.into_data_stream()).await.map_err(r2_error)?.ok_or_else(|| {
            ApiError::new(http::StatusCode::PRECONDITION_FAILED, 10031, "Precondition failed")
        })?;
    Ok(ok(object_json(&o, false, false)))
}

async fn get_object(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, key)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let b = Bucket { db: &state.db, blobs: &state.blobs, id: bucket_id(&state, &id, &bucket).await? };
    let GetResult::Found(o, _) = b.get(&key, &json!({})).await.map_err(r2_error)? else {
        return Err(ApiError::new(http::StatusCode::NOT_FOUND, 10007, "The specified key does not exist."));
    };
    let mut resp = Body::from_stream(read_object(&state.blobs, &o.parts, 0, o.size as u64)).into_response();
    let h = resp.headers_mut();
    for (header, field) in HTTP_HEADERS {
        if let Some(v) = o.http_fields.get(*field).and_then(Value::as_str).and_then(|v| v.parse().ok()) {
            h.insert(*header, v);
        }
    }
    if let Ok(v) = format!("\"{}\"", o.etag).parse() {
        h.insert("etag", v);
    }
    h.insert("content-length", o.size.into());
    Ok(resp)
}

async fn delete_object(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, key)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let b = Bucket { db: &state.db, blobs: &state.blobs, id: bucket_id(&state, &id, &bucket).await? };
    b.delete(vec![key]).await.map_err(r2_error)?;
    Ok(ok(Value::Null))
}

#[cfg(test)]
mod tests {
    #[test]
    fn bucket_names() {
        assert!(super::valid_bucket_name("my-bucket-1"));
        assert!(!super::valid_bucket_name("ab"));
        assert!(!super::valid_bucket_name("-ab"));
        assert!(!super::valid_bucket_name("My-Bucket"));
        assert_eq!(&super::iso(std::time::UNIX_EPOCH)[..19], "1970-01-01T00:00:00");
    }
}

// ---- public access: the r2.dev-style URL and custom domains ----

fn dev_url_json(state: &State, bucket: &str, url: &selvedge_store::r2public::DevUrl) -> Value {
    let domain = state.r2_dev_zone.as_deref().map(|z| format!("pub-{}.{z}", url.id)).unwrap_or_default();
    json!({ "bucketId": bucket, "domain": domain, "enabled": url.enabled && state.r2_dev_zone.is_some() })
}

async fn get_dev_url(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = bucket_id(&state, &id, &bucket).await?;
    let url = state.db.r2_dev_url(&bucket_id).await?;
    Ok(ok(dev_url_json(&state, &bucket_id, &url)))
}

#[derive(Deserialize)]
struct DevUrlBody {
    enabled: bool,
}

async fn put_dev_url(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket)): Path<(String, String)>,
    super::JsonBody(body): super::JsonBody<DevUrlBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = bucket_id(&state, &id, &bucket).await?;
    if body.enabled && state.r2_dev_zone.is_none() {
        return Err(ApiError::bad_request(
            "public bucket URLs are not set up on this platform (the operator sets SELVEDGE_R2_DEV_ZONE)",
        ));
    }
    let url = state.db.set_r2_dev_url(&bucket_id, Some(body.enabled)).await?;
    Ok(ok(dev_url_json(&state, &bucket_id, &url)))
}

fn custom_json(d: &CustomDomain) -> Value {
    json!({
        "domain": d.domain,
        "enabled": d.enabled,
        "minTLS": d.min_tls,
        "zoneId": d.zone_id,
        "zoneName": d.zone_name,
        "status": { "ownership": "active", "ssl": "active" },
    })
}

fn no_domain() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 10102, "The custom domain is not connected to this bucket.")
}

fn valid_min_tls(v: &str) -> bool {
    matches!(v, "1.0" | "1.1" | "1.2" | "1.3")
}

async fn list_custom(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = bucket_id(&state, &id, &bucket).await?;
    let domains = state.db.r2_custom_domains(&bucket_id).await?;
    Ok(ok(json!({ "domains": domains.iter().map(custom_json).collect::<Vec<_>>() })))
}

async fn get_custom(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, domain)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = bucket_id(&state, &id, &bucket).await?;
    let d =
        state.db.r2_custom_domain(&bucket_id, &domain.to_ascii_lowercase()).await?.ok_or_else(no_domain)?;
    Ok(ok(custom_json(&d)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddCustomBody {
    domain: String,
    zone_id: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(rename = "minTLS")]
    min_tls: Option<String>,
}

fn yes() -> bool {
    true
}

async fn add_custom(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket)): Path<(String, String)>,
    super::JsonBody(body): super::JsonBody<AddCustomBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = bucket_id(&state, &id, &bucket).await?;
    let domain = body.domain.trim_end_matches('.').to_ascii_lowercase();
    if !super::domains::valid_hostname(&domain) {
        return Err(ApiError::bad_request(format!("`{domain}` is not a valid hostname")));
    }
    let min_tls = body.min_tls.unwrap_or_else(|| "1.0".to_owned());
    if !valid_min_tls(&min_tls) {
        return Err(ApiError::bad_request("minTLS must be one of 1.0, 1.1, 1.2 and 1.3"));
    }
    // The zone stands for ownership of the domain: an operator assigned it.
    let zone = state.db.zone(&body.zone_id).await?.filter(|z| z.account_id == id).ok_or_else(|| {
        ApiError::new(http::StatusCode::NOT_FOUND, 10103, "The zone does not exist in this account.")
    })?;
    if domain != zone.name && !domain.ends_with(&format!(".{}", zone.name)) {
        return Err(ApiError::bad_request(format!("`{domain}` is not in the zone `{}`", zone.name)));
    }
    if !state.db.add_r2_custom_domain(&bucket_id, &domain, &zone.id, body.enabled, &min_tls).await? {
        return Err(ApiError::new(
            http::StatusCode::CONFLICT,
            10104,
            "The domain is already in use by a bucket or a Worker.",
        ));
    }
    let d = state.db.r2_custom_domain(&bucket_id, &domain).await?.ok_or_else(no_domain)?;
    Ok(ok(custom_json(&d)))
}

#[derive(Deserialize)]
struct UpdateCustomBody {
    enabled: Option<bool>,
    #[serde(rename = "minTLS")]
    min_tls: Option<String>,
}

async fn update_custom(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, domain)): Path<(String, String, String)>,
    super::JsonBody(body): super::JsonBody<UpdateCustomBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = bucket_id(&state, &id, &bucket).await?;
    if body.min_tls.as_deref().is_some_and(|v| !valid_min_tls(v)) {
        return Err(ApiError::bad_request("minTLS must be one of 1.0, 1.1, 1.2 and 1.3"));
    }
    let d = state
        .db
        .update_r2_custom_domain(
            &bucket_id,
            &domain.to_ascii_lowercase(),
            body.enabled,
            body.min_tls.as_deref(),
        )
        .await?
        .ok_or_else(no_domain)?;
    Ok(ok(custom_json(&d)))
}

async fn delete_custom(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, bucket, domain)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let bucket_id = bucket_id(&state, &id, &bucket).await?;
    let domain = domain.to_ascii_lowercase();
    if !state.db.delete_r2_custom_domain(&bucket_id, &domain).await? {
        return Err(no_domain());
    }
    Ok(ok(json!({ "domain": domain })))
}
