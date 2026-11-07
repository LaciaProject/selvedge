//! R2's S3-compatible API, for S3 clients (the AWS SDKs, rclone, ...).
//!
//! Path-style addressing (`/<bucket>/<key>`) on its own listener in the data
//! service, over the same buckets and objects as the binding. Requests are
//! signed with SigV4, in headers or as presigned URLs; as on Cloudflare, the
//! access key id is an account API token's id and the secret is the SHA-256 of
//! the token's value. Bodies may be `aws-chunked` (what current SDKs send for
//! uploads); chunk signatures are not checked, the request signature is.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State as AxState};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt, TryStreamExt};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use percent_encoding::{percent_decode_str, utf8_percent_encode};
use selvedge_core::AccountId;
use selvedge_store::r2::{BucketDelete, R2Object};
use selvedge_store::{Blobs, Db};
use serde_json::{Value, json};

use crate::r2::{Bucket, GetResult, R2Error, read_object};
use crate::sigv4::{AWS_PATH, amz_date_secs, parse_authorization, signature};

const MAX_SKEW_SECS: i64 = 15 * 60;
const MAX_PRESIGN_SECS: i64 = 7 * 24 * 3600;
const MAX_XML_BODY: usize = 4 << 20;

pub struct S3State {
    pub db: Db,
    pub blobs: Blobs,
}

pub fn router(state: Arc<S3State>) -> axum::Router {
    axum::Router::new().fallback(handle).layer(axum::extract::DefaultBodyLimit::disable()).with_state(state)
}

// ---- responses ----

struct S3Error {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl S3Error {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }
    fn denied(message: &str) -> Self {
        Self::new(StatusCode::FORBIDDEN, "AccessDenied", message)
    }
    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "InvalidArgument", message)
    }
    fn no_bucket() -> Self {
        Self::new(StatusCode::NOT_FOUND, "NoSuchBucket", "The specified bucket does not exist.")
    }
    fn no_key() -> Self {
        Self::new(StatusCode::NOT_FOUND, "NoSuchKey", "The specified key does not exist.")
    }
}

impl From<R2Error> for S3Error {
    fn from(e: R2Error) -> Self {
        let code = match (e.status.as_u16(), e.code) {
            (404, 10007) => "NoSuchKey",
            (404, _) => "NoSuchUpload",
            (412, _) => "PreconditionFailed",
            (416, _) => "InvalidRange",
            (400, 10037) => "BadDigest",
            (400, 10011) => "EntityTooSmall",
            (400, 10025) => "InvalidPart",
            (400, 10020) => "InvalidObjectName",
            (500, _) => "InternalError",
            _ => "InvalidRequest",
        };
        Self { status: e.status, code, message: e.message }
    }
}

impl From<anyhow::Error> for S3Error {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %format!("{e:#}"), "R2 S3 API internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", "We encountered an internal error.")
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{}</Code><Message>{}</Message></Error>",
            self.code,
            esc(&self.message)
        );
        (self.status, [(http::header::CONTENT_TYPE, "application/xml")], body).into_response()
    }
}

type S3Result<T> = Result<T, S3Error>;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml(status: StatusCode, body: String) -> Response {
    (
        status,
        [(http::header::CONTENT_TYPE, "application/xml")],
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{body}"),
    )
        .into_response()
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn http_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn parse_http_date(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(s).ok().map(|d| d.timestamp_millis())
}

// ---- authentication ----

/// How the request body is encoded and what it was signed with.
enum Payload {
    /// The body's SHA-256, to check while storing.
    Hash(String),
    Unsigned,
    /// `aws-chunked`.
    Chunked,
}

fn query_pairs(query: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

async fn authenticate(
    st: &S3State,
    method: &Method,
    path: &str,
    query: &str,
    headers: &HeaderMap,
) -> S3Result<(AccountId, Payload)> {
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let decoded = percent_decode_str(path).decode_utf8().map_err(|_| S3Error::bad("invalid path"))?;
    let canonical_path = utf8_percent_encode(&decoded, AWS_PATH).to_string();
    let now = chrono::Utc::now().timestamp();
    let signed_values = |names: &[String]| -> S3Result<Vec<(String, String)>> {
        names
            .iter()
            .map(|n| {
                let v = h(n).ok_or_else(|| S3Error::denied("a signed header is missing"))?;
                Ok((n.clone(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
            })
            .collect()
    };

    if let Some(auth) = h("authorization") {
        let a = parse_authorization(auth).ok_or_else(|| S3Error::denied("unsupported authorization"))?;
        let (secret, account) = st.db.s3_credentials(&a.access_key_id).await?.ok_or_else(|| {
            S3Error::new(StatusCode::FORBIDDEN, "InvalidAccessKeyId", "The access key id does not exist.")
        })?;
        let amz_date = h("x-amz-date").ok_or_else(|| S3Error::denied("missing x-amz-date"))?;
        if amz_date_secs(amz_date).is_none_or(|t| (t - now).abs() > MAX_SKEW_SECS) {
            return Err(S3Error::new(
                StatusCode::FORBIDDEN,
                "RequestTimeTooSkewed",
                "The difference between the request time and the current time is too large.",
            ));
        }
        let payload_hash =
            h("x-amz-content-sha256").ok_or_else(|| S3Error::bad("missing x-amz-content-sha256"))?;
        let expected = signature(
            &secret,
            method.as_str(),
            &canonical_path,
            query,
            &signed_values(&a.signed_headers)?,
            payload_hash,
            amz_date,
            &a.region,
        );
        if !crate::constant_time_eq(expected.as_bytes(), a.signature.as_bytes()) {
            return Err(S3Error::new(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            ));
        }
        let payload = match payload_hash {
            "UNSIGNED-PAYLOAD" => Payload::Unsigned,
            p if p.starts_with("STREAMING-") => Payload::Chunked,
            p if p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Payload::Hash(p.to_ascii_lowercase())
            }
            _ => return Err(S3Error::bad("invalid x-amz-content-sha256")),
        };
        return Ok((account, payload));
    }

    // A presigned URL.
    let pairs = query_pairs(query);
    let q = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let (Some(alg), Some(cred), Some(amz_date), Some(expires), Some(signed), Some(sig)) = (
        q("X-Amz-Algorithm"),
        q("X-Amz-Credential"),
        q("X-Amz-Date"),
        q("X-Amz-Expires"),
        q("X-Amz-SignedHeaders"),
        q("X-Amz-Signature"),
    ) else {
        return Err(S3Error::denied("Anonymous access is not allowed."));
    };
    if alg != "AWS4-HMAC-SHA256" {
        return Err(S3Error::denied("unsupported X-Amz-Algorithm"));
    }
    let mut c = cred.split('/');
    let (Some(akid), Some(_), Some(region)) = (c.next(), c.next(), c.next()) else {
        return Err(S3Error::denied("invalid X-Amz-Credential"));
    };
    let (secret, account) = st.db.s3_credentials(akid).await?.ok_or_else(|| {
        S3Error::new(StatusCode::FORBIDDEN, "InvalidAccessKeyId", "The access key id does not exist.")
    })?;
    let start = amz_date_secs(amz_date).ok_or_else(|| S3Error::denied("invalid X-Amz-Date"))?;
    let expires: i64 = expires.parse().map_err(|_| S3Error::denied("invalid X-Amz-Expires"))?;
    if !(1..=MAX_PRESIGN_SECS).contains(&expires) || now > start + expires || start > now + MAX_SKEW_SECS {
        return Err(S3Error::denied("Request has expired"));
    }
    // The canonical query is the signed one: everything but the signature.
    let unsigned: String =
        query.split('&').filter(|p| !p.starts_with("X-Amz-Signature=")).collect::<Vec<_>>().join("&");
    let names: Vec<String> = signed.split(';').map(str::to_owned).collect();
    let expected = signature(
        &secret,
        method.as_str(),
        &canonical_path,
        &unsigned,
        &signed_values(&names)?,
        "UNSIGNED-PAYLOAD",
        amz_date,
        region,
    );
    if !crate::constant_time_eq(expected.as_bytes(), sig.as_bytes()) {
        return Err(S3Error::new(
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
        ));
    }
    Ok((account, Payload::Unsigned))
}

// ---- aws-chunked bodies ----

/// Decodes an `aws-chunked` body: `<hex size>[;chunk-signature=...]\r\n<data>\r\n`
/// until a zero-size chunk, then optional trailers.
fn dechunk(
    body: impl Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send + 'static,
) -> impl Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send {
    struct St<S> {
        input: S,
        buf: BytesMut,
        remaining: usize,
        done: bool,
    }
    let bad = |m: &str| axum::Error::new(std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned()));
    Box::pin(futures::stream::unfold(
        St { input: body, buf: BytesMut::new(), remaining: 0, done: false },
        move |mut st| async move {
            loop {
                if st.done {
                    return None;
                }
                if st.remaining > 0 {
                    if st.buf.is_empty() {
                        match st.input.next().await {
                            Some(Ok(b)) => st.buf.extend_from_slice(&b),
                            Some(Err(e)) => return Some((Err(e), st)),
                            None => {
                                return Some((
                                    Err(bad("truncated aws-chunked body")),
                                    St { done: true, ..st },
                                ));
                            }
                        }
                    }
                    let n = st.remaining.min(st.buf.len());
                    let out = st.buf.split_to(n).freeze();
                    st.remaining -= n;
                    return Some((Ok(out), st));
                }
                // A chunk header (after the previous chunk's CRLF).
                while st.buf.starts_with(b"\r\n") {
                    let _ = st.buf.split_to(2);
                }
                let Some(eol) = st.buf.windows(2).position(|w| w == b"\r\n") else {
                    match st.input.next().await {
                        Some(Ok(b)) => {
                            st.buf.extend_from_slice(&b);
                            if st.buf.len() > 64 << 10 {
                                return Some((
                                    Err(bad("aws-chunked header too long")),
                                    St { done: true, ..st },
                                ));
                            }
                            continue;
                        }
                        Some(Err(e)) => return Some((Err(e), st)),
                        None => {
                            return Some((Err(bad("truncated aws-chunked body")), St { done: true, ..st }));
                        }
                    }
                };
                let line = String::from_utf8_lossy(&st.buf[..eol]).to_string();
                let _ = st.buf.split_to(eol + 2);
                let size_hex = line.split(';').next().unwrap_or("").trim();
                let Ok(size) = usize::from_str_radix(size_hex, 16) else {
                    return Some((Err(bad("invalid aws-chunked size")), St { done: true, ..st }));
                };
                if size == 0 {
                    // Trailers (checksums) follow; the object's MD5 is computed anyway.
                    return None;
                }
                st.remaining = size;
            }
        },
    ))
}

fn body_stream(
    body: Body,
    payload: &Payload,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>> {
    let raw = body.into_data_stream();
    match payload {
        Payload::Chunked => Box::pin(dechunk(raw)),
        _ => Box::pin(raw),
    }
}

// ---- metadata ----

const HTTP_FIELDS: [(&str, &str); 6] = [
    ("content-type", "contentType"),
    ("content-language", "contentLanguage"),
    ("content-disposition", "contentDisposition"),
    ("content-encoding", "contentEncoding"),
    ("cache-control", "cacheControl"),
    ("expires", "cacheExpiry"),
];

/// The R2 put request (`httpFields`, `customFields`, checks, conditions) for
/// an S3 write's headers.
fn put_request(headers: &HeaderMap, payload: &Payload) -> Value {
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let mut http = serde_json::Map::new();
    for (header, field) in HTTP_FIELDS {
        if let Some(v) = h(header) {
            // aws-chunked describes the upload, not the object.
            let v = if header == "content-encoding" {
                v.split(',').map(str::trim).filter(|e| *e != "aws-chunked").collect::<Vec<_>>().join(", ")
            } else {
                v.to_owned()
            };
            if v.is_empty() {
                continue;
            }
            if field == "cacheExpiry" {
                if let Some(ms) = parse_http_date(&v) {
                    http.insert(field.into(), json!(ms));
                }
            } else {
                http.insert(field.into(), json!(v));
            }
        }
    }
    let custom: Vec<Value> = headers
        .iter()
        .filter_map(|(k, v)| {
            let name = k.as_str().strip_prefix("x-amz-meta-")?;
            Some(json!({ "k": name, "v": v.to_str().ok()? }))
        })
        .collect();
    let mut req = json!({ "httpFields": http, "customFields": custom });
    if let Some(md5) = h("content-md5") {
        req["md5"] = json!(md5);
    }
    if let Payload::Hash(sha) = payload {
        req["sha256"] = json!(sha);
    }
    let mut only_if = serde_json::Map::new();
    if let Some(m) = h("if-match") {
        only_if.insert("etagMatches".into(), etag_list(m));
    }
    if let Some(m) = h("if-none-match") {
        only_if.insert("etagDoesNotMatch".into(), etag_list(m));
    }
    if !only_if.is_empty() {
        req["onlyIf"] = Value::Object(only_if);
    }
    req
}

fn etag_list(header: &str) -> Value {
    json!(
        header
            .split(',')
            .map(str::trim)
            .map(|e| if e == "*" {
                json!({ "type": "wildcard" })
            } else {
                json!({ "type": "strong", "value": e.trim_matches('"') })
            })
            .collect::<Vec<_>>()
    )
}

fn object_headers(o: &R2Object, headers: &mut HeaderMap) {
    let mut set = |k: &str, v: String| {
        if let (Ok(name), Ok(value)) = (http::HeaderName::try_from(k), HeaderValue::from_str(&v)) {
            headers.insert(name, value);
        }
    };
    set("etag", format!("\"{}\"", o.etag));
    set("last-modified", http_date(o.uploaded_ms));
    set("accept-ranges", "bytes".into());
    for (header, field) in HTTP_FIELDS {
        match (field, &o.http_fields[field]) {
            ("cacheExpiry", Value::Number(n)) => set(header, http_date(n.as_i64().unwrap_or_default())),
            (_, Value::String(s)) => set(header, s.clone()),
            _ => {}
        }
    }
    if !o.http_fields["contentType"].is_string() {
        set("content-type", "application/octet-stream".into());
    }
    for f in o.custom_fields.as_array().into_iter().flatten() {
        if let (Some(k), Some(v)) = (f["k"].as_str(), f["v"].as_str()) {
            set(&format!("x-amz-meta-{k}"), v.to_owned());
        }
    }
}

/// S3's read preconditions: `Err` with 304 or 412 when one fails.
fn read_conditions(headers: &HeaderMap, o: &R2Object) -> Result<(), StatusCode> {
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let matches =
        |list: &str| list.split(',').map(str::trim).any(|e| e == "*" || e.trim_matches('"') == o.etag);
    let secs = o.uploaded_ms / 1000;
    if let Some(m) = h("if-match") {
        if !matches(m) {
            return Err(StatusCode::PRECONDITION_FAILED);
        }
    } else if let Some(t) = h("if-unmodified-since").and_then(parse_http_date) {
        if secs > t / 1000 {
            return Err(StatusCode::PRECONDITION_FAILED);
        }
    }
    if let Some(m) = h("if-none-match") {
        if matches(m) {
            return Err(StatusCode::NOT_MODIFIED);
        }
    } else if let Some(t) = h("if-modified-since").and_then(parse_http_date) {
        if secs <= t / 1000 {
            return Err(StatusCode::NOT_MODIFIED);
        }
    }
    Ok(())
}

// ---- the handler ----

async fn handle(AxState(st): AxState<Arc<S3State>>, req: Request) -> Response {
    match dispatch(&st, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn dispatch(st: &S3State, req: Request) -> S3Result<Response> {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_owned();
    let query = parts.uri.query().unwrap_or("").to_owned();
    let (account, payload) = authenticate(st, &parts.method, &path, &query, &parts.headers).await?;
    let pairs = query_pairs(&query);
    let has = |k: &str| pairs.iter().any(|(n, _)| n == k);
    let q = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    let decoded =
        percent_decode_str(&path).decode_utf8().map_err(|_| S3Error::bad("invalid path"))?.into_owned();
    let rest = decoded.trim_start_matches('/');
    let (bucket_name, key) = match rest.split_once('/') {
        Some((b, k)) if !k.is_empty() => (b.to_owned(), Some(k.to_owned())),
        Some((b, _)) => (b.to_owned(), None),
        None => (rest.to_owned(), None),
    };
    let method = parts.method.clone();
    let headers = &parts.headers;

    if bucket_name.is_empty() {
        return if method == Method::GET {
            list_buckets(st, &account).await
        } else {
            Err(S3Error::bad("unsupported request"))
        };
    }
    let Some(key) = key else {
        return match method {
            Method::PUT => create_bucket(st, &account, &bucket_name).await,
            Method::DELETE => delete_bucket(st, &account, &bucket_name).await,
            _ => {
                let bucket = open(st, &account, &bucket_name).await?;
                match method {
                    Method::HEAD => Ok(StatusCode::OK.into_response()),
                    Method::GET if has("location") => {
                        Ok(xml(StatusCode::OK, "<LocationConstraint>auto</LocationConstraint>".into()))
                    }
                    Method::GET if has("uploads") => Ok(xml(
                        StatusCode::OK,
                        format!(
                            "<ListMultipartUploadsResult><Bucket>{}</Bucket><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>",
                            esc(&bucket_name)
                        ),
                    )),
                    Method::GET => list_objects(&bucket, &bucket_name, &pairs).await,
                    Method::POST if has("delete") => delete_objects(&bucket, body).await,
                    _ => Err(S3Error::bad("unsupported bucket request")),
                }
            }
        };
    };
    let bucket = open(st, &account, &bucket_name).await?;
    match method {
        Method::GET if has("uploadId") => {
            list_parts(st, &bucket, &bucket_name, &key, &q("uploadId").unwrap_or_default()).await
        }
        Method::GET | Method::HEAD => get_object(&bucket, &key, headers, method == Method::HEAD).await,
        Method::PUT if has("uploadId") => {
            let number: u64 = q("partNumber")
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| S3Error::bad("invalid partNumber"))?;
            let etag = bucket
                .upload_part(&key, &q("uploadId").unwrap_or_default(), number, body_stream(body, &payload))
                .await?;
            Ok((StatusCode::OK, [("etag", format!("\"{etag}\""))]).into_response())
        }
        Method::PUT if headers.contains_key("x-amz-copy-source") => {
            copy_object(st, &account, &bucket, &key, headers).await
        }
        Method::PUT => {
            let req = put_request(headers, &payload);
            match bucket.put(&key, &req, body_stream(body, &payload)).await? {
                Some(o) => Ok((StatusCode::OK, [("etag", format!("\"{}\"", o.etag))]).into_response()),
                None => Err(S3Error::new(
                    StatusCode::PRECONDITION_FAILED,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                )),
            }
        }
        Method::DELETE if has("uploadId") => {
            bucket.abort_upload(&key, &q("uploadId").unwrap_or_default()).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Method::DELETE => {
            bucket.delete(vec![key]).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Method::POST if has("uploads") => {
            let id = bucket.create_upload(&key, &put_request(headers, &Payload::Unsigned)).await?;
            Ok(xml(
                StatusCode::OK,
                format!(
                    "<InitiateMultipartUploadResult><Bucket>{}</Bucket><Key>{}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>",
                    esc(&bucket_name),
                    esc(&key)
                ),
            ))
        }
        Method::POST if has("uploadId") => {
            complete_upload(&bucket, &bucket_name, &key, &q("uploadId").unwrap_or_default(), body).await
        }
        _ => Err(S3Error::bad("unsupported object request")),
    }
}

/// The bucket an S3 request names, owned by the caller's account.
async fn open<'a>(st: &'a S3State, account: &AccountId, name: &str) -> S3Result<Bucket<'a>> {
    let (id, _) = st.db.r2_bucket(account, name).await?.ok_or_else(S3Error::no_bucket)?;
    Ok(Bucket { db: &st.db, blobs: &st.blobs, id })
}

async fn list_buckets(st: &S3State, account: &AccountId) -> S3Result<Response> {
    let buckets = st.db.r2_buckets(account).await?;
    let items: String = buckets
        .iter()
        .map(|(name, created)| {
            let ms = created
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_default();
            format!("<Bucket><Name>{}</Name><CreationDate>{}</CreationDate></Bucket>", esc(name), iso(ms))
        })
        .collect();
    Ok(xml(
        StatusCode::OK,
        format!(
            "<ListAllMyBucketsResult><Owner><ID>{account}</ID><DisplayName>{account}</DisplayName></Owner><Buckets>{items}</Buckets></ListAllMyBucketsResult>"
        ),
    ))
}

async fn create_bucket(st: &S3State, account: &AccountId, name: &str) -> S3Result<Response> {
    if !crate::api::valid_bucket_name(name) {
        return Err(S3Error::new(
            StatusCode::BAD_REQUEST,
            "InvalidBucketName",
            "The specified bucket is not valid.",
        ));
    }
    match st.db.create_r2_bucket(account, name).await? {
        Some(_) => Ok((StatusCode::OK, [("location", format!("/{name}"))]).into_response()),
        None => Err(S3Error::new(
            StatusCode::CONFLICT,
            "BucketAlreadyOwnedByYou",
            "Your previous request to create the named bucket succeeded and you already own it.",
        )),
    }
}

async fn delete_bucket(st: &S3State, account: &AccountId, name: &str) -> S3Result<Response> {
    match st.db.delete_r2_bucket(account, name).await? {
        BucketDelete::Deleted => Ok(StatusCode::NO_CONTENT.into_response()),
        BucketDelete::NotFound => Err(S3Error::no_bucket()),
        BucketDelete::NotEmpty => Err(S3Error::new(
            StatusCode::CONFLICT,
            "BucketNotEmpty",
            "The bucket you tried to delete is not empty.",
        )),
    }
}

async fn list_objects(bucket: &Bucket<'_>, name: &str, pairs: &[(String, String)]) -> S3Result<Response> {
    let q = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let v2 = q("list-type") == Some("2");
    let max_keys: u64 = q("max-keys").and_then(|m| m.parse().ok()).unwrap_or(1000).min(1000);
    let prefix = q("prefix").unwrap_or("");
    let delimiter = q("delimiter").filter(|d| !d.is_empty());
    let mut req = json!({ "prefix": prefix, "limit": max_keys.max(1), "include": ["httpMetadata"] });
    if let Some(d) = delimiter {
        req["delimiter"] = json!(d);
    }
    if v2 {
        if let Some(c) = q("continuation-token") {
            req["cursor"] = json!(c);
        } else if let Some(s) = q("start-after") {
            req["startAfter"] = json!(s);
        }
    } else if let Some(m) = q("marker") {
        req["startAfter"] = json!(m);
    }
    let result = if max_keys == 0 {
        json!({ "objects": [], "delimitedPrefixes": [], "truncated": false })
    } else {
        bucket.list(&req).await?
    };
    let objects = result["objects"].as_array().cloned().unwrap_or_default();
    let prefixes = result["delimitedPrefixes"].as_array().cloned().unwrap_or_default();
    let truncated = result["truncated"].as_bool().unwrap_or(false);
    let mut out = format!(
        "<ListBucketResult><Name>{}</Name><Prefix>{}</Prefix><MaxKeys>{max_keys}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
        esc(name),
        esc(prefix)
    );
    if let Some(d) = delimiter {
        out.push_str(&format!("<Delimiter>{}</Delimiter>", esc(d)));
    }
    if v2 {
        out.push_str(&format!("<KeyCount>{}</KeyCount>", objects.len() + prefixes.len()));
        if let Some(c) = q("continuation-token") {
            out.push_str(&format!("<ContinuationToken>{}</ContinuationToken>", esc(c)));
        }
        if let (true, Some(c)) = (truncated, result["cursor"].as_str()) {
            out.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", esc(c)));
        }
    } else {
        out.push_str(&format!("<Marker>{}</Marker>", esc(q("marker").unwrap_or(""))));
        let last = objects
            .last()
            .and_then(|o| o["name"].as_str())
            .or_else(|| prefixes.last().and_then(Value::as_str));
        if let (true, Some(l)) = (truncated, last) {
            out.push_str(&format!("<NextMarker>{}</NextMarker>", esc(l)));
        }
    }
    for o in &objects {
        out.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            esc(o["name"].as_str().unwrap_or("")),
            iso(o["uploaded"].as_i64().unwrap_or_default()),
            esc(o["etag"].as_str().unwrap_or("")),
            o["size"].as_i64().unwrap_or_default()
        ));
    }
    for p in &prefixes {
        out.push_str(&format!(
            "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            esc(p.as_str().unwrap_or(""))
        ));
    }
    out.push_str("</ListBucketResult>");
    Ok(xml(StatusCode::OK, out))
}

async fn get_object(bucket: &Bucket<'_>, key: &str, headers: &HeaderMap, head: bool) -> S3Result<Response> {
    let o = bucket.head(key).await?.ok_or_else(S3Error::no_key)?;
    if let Err(status) = read_conditions(headers, &o) {
        let mut r = status.into_response();
        object_headers(&o, r.headers_mut());
        return Ok(r);
    }
    let range = headers.get("range").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let req = json!({ "rangeHeader": range });
    let (o, range) = match bucket.get(key, &req).await? {
        GetResult::Found(o, r) => (o, r),
        GetResult::NotFound => return Err(S3Error::no_key()),
        GetResult::PreconditionFailed(_) => {
            return Err(S3Error::new(
                StatusCode::PRECONDITION_FAILED,
                "PreconditionFailed",
                "precondition failed",
            ));
        }
    };
    let size = o.size as u64;
    let (start, len) = range.unwrap_or((0, size));
    let body =
        if head { Body::empty() } else { Body::from_stream(read_object(bucket.blobs, &o.parts, start, len)) };
    let mut r = Response::new(body);
    object_headers(&o, r.headers_mut());
    r.headers_mut().insert(http::header::CONTENT_LENGTH, HeaderValue::from(len));
    if range.is_some() {
        *r.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + len.max(1) - 1)) {
            r.headers_mut().insert(http::header::CONTENT_RANGE, v);
        }
    }
    Ok(r)
}

async fn copy_object(
    st: &S3State,
    account: &AccountId,
    bucket: &Bucket<'_>,
    key: &str,
    headers: &HeaderMap,
) -> S3Result<Response> {
    let source = headers.get("x-amz-copy-source").and_then(|v| v.to_str().ok()).unwrap_or("");
    let source = percent_decode_str(source.split('?').next().unwrap_or("")).decode_utf8_lossy().to_string();
    let (src_bucket, src_key) = source
        .trim_start_matches('/')
        .split_once('/')
        .ok_or_else(|| S3Error::bad("invalid x-amz-copy-source"))?;
    let src = open(st, account, src_bucket).await?;
    let o = src.head(src_key).await?.ok_or_else(S3Error::no_key)?;
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(m) = h("x-amz-copy-source-if-match") {
        if !m.split(',').any(|e| e.trim().trim_matches('"') == o.etag) {
            return Err(S3Error::new(
                StatusCode::PRECONDITION_FAILED,
                "PreconditionFailed",
                "x-amz-copy-source-if-match did not hold",
            ));
        }
    }
    if let Some(m) = h("x-amz-copy-source-if-none-match") {
        if m.split(',').any(|e| e.trim().trim_matches('"') == o.etag) {
            return Err(S3Error::new(
                StatusCode::PRECONDITION_FAILED,
                "PreconditionFailed",
                "x-amz-copy-source-if-none-match did not hold",
            ));
        }
    }
    let req = if h("x-amz-metadata-directive") == Some("REPLACE") {
        put_request(headers, &Payload::Unsigned)
    } else {
        let mut r = json!({ "httpFields": o.http_fields, "customFields": o.custom_fields });
        if let Value::Object(c) = &put_request(headers, &Payload::Unsigned)["onlyIf"] {
            r["onlyIf"] = Value::Object(c.clone());
        }
        r
    };
    let stream = read_object(&st.blobs, &o.parts, 0, o.size as u64).map_err(axum::Error::new);
    let copied = bucket.put(key, &req, Box::pin(stream)).await?.ok_or_else(|| {
        S3Error::new(StatusCode::PRECONDITION_FAILED, "PreconditionFailed", "precondition failed")
    })?;
    Ok(xml(
        StatusCode::OK,
        format!(
            "<CopyObjectResult><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag></CopyObjectResult>",
            iso(copied.uploaded_ms),
            copied.etag
        ),
    ))
}

async fn read_xml(body: Body) -> S3Result<String> {
    let bytes =
        axum::body::to_bytes(body, MAX_XML_BODY).await.map_err(|_| S3Error::bad("request body too large"))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        S3Error::new(StatusCode::BAD_REQUEST, "MalformedXML", "The XML you provided was not well-formed.")
    })
}

/// The text of every `<tag>` element, in order (tags are not nested within
/// themselves in the bodies S3 clients send).
fn xml_texts(doc: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(unescape(&after[..j]));
        rest = &after[j + close.len()..];
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

async fn delete_objects(bucket: &Bucket<'_>, body: Body) -> S3Result<Response> {
    let doc = read_xml(body).await?;
    let keys: Vec<String> = xml_texts(&doc, "Object").iter().flat_map(|o| xml_texts(o, "Key")).collect();
    if keys.is_empty() || keys.len() > 1000 {
        return Err(S3Error::new(StatusCode::BAD_REQUEST, "MalformedXML", "Delete needs 1 to 1000 keys."));
    }
    let quiet = xml_texts(&doc, "Quiet").first().is_some_and(|q| q.trim() == "true");
    bucket.delete(keys.clone()).await?;
    let deleted: String = if quiet {
        String::new()
    } else {
        keys.iter().map(|k| format!("<Deleted><Key>{}</Key></Deleted>", esc(k))).collect()
    };
    Ok(xml(StatusCode::OK, format!("<DeleteResult>{deleted}</DeleteResult>")))
}

async fn complete_upload(
    bucket: &Bucket<'_>,
    name: &str,
    key: &str,
    upload: &str,
    body: Body,
) -> S3Result<Response> {
    let doc = read_xml(body).await?;
    let parts: Vec<Value> = xml_texts(&doc, "Part")
        .iter()
        .map(|p| {
            json!({
                "part": xml_texts(p, "PartNumber").first().and_then(|n| n.trim().parse::<u64>().ok()),
                "etag": xml_texts(p, "ETag").first().map(|e| e.trim().trim_matches('"').to_owned()),
            })
        })
        .collect();
    let o = bucket.complete_upload(key, upload, &json!(parts)).await?;
    Ok(xml(
        StatusCode::OK,
        format!(
            "<CompleteMultipartUploadResult><Location>/{}/{}</Location><Bucket>{}</Bucket><Key>{}</Key><ETag>&quot;{}&quot;</ETag></CompleteMultipartUploadResult>",
            esc(name),
            esc(key),
            esc(name),
            esc(key),
            o.etag
        ),
    ))
}

async fn list_parts(
    st: &S3State,
    bucket: &Bucket<'_>,
    name: &str,
    key: &str,
    upload: &str,
) -> S3Result<Response> {
    let u = st.db.r2_upload(&bucket.id, upload).await?.filter(|u| u.key == key).ok_or_else(|| {
        S3Error::new(StatusCode::NOT_FOUND, "NoSuchUpload", "The specified multipart upload does not exist.")
    })?;
    let parts: String = st
        .db
        .r2_upload_parts(&u.id)
        .await?
        .iter()
        .map(|p| {
            format!(
                "<Part><PartNumber>{}</PartNumber><ETag>&quot;{}&quot;</ETag><Size>{}</Size></Part>",
                p.part, p.etag, p.size
            )
        })
        .collect();
    Ok(xml(
        StatusCode::OK,
        format!(
            "<ListPartsResult><Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId><IsTruncated>false</IsTruncated>{parts}</ListPartsResult>",
            esc(name),
            esc(key),
            esc(upload)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn aws_chunked_bodies() {
        let raw = b"5;chunk-signature=abc\r\nhello\r\n6\r\n world\r\n0\r\nx-amz-checksum-crc32:AAAA\r\n\r\n";
        // Split awkwardly, as the network does.
        let pieces: Vec<Result<Bytes, axum::Error>> =
            raw.chunks(3).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        let out: Vec<Bytes> = dechunk(futures::stream::iter(pieces)).try_collect().await.unwrap();
        assert_eq!(out.concat(), b"hello world");
        let truncated = vec![Ok(Bytes::from_static(b"5\r\nhel"))];
        assert!(dechunk(futures::stream::iter(truncated)).try_collect::<Vec<_>>().await.is_err());
    }

    #[test]
    fn xml_parsing() {
        let doc = "<Delete><Object><Key>a&amp;b</Key></Object><Object><Key>c</Key></Object><Quiet>true</Quiet></Delete>";
        let keys: Vec<String> = xml_texts(doc, "Object").iter().flat_map(|o| xml_texts(o, "Key")).collect();
        assert_eq!(keys, ["a&b", "c"]);
        assert_eq!(esc("<a&\"b'>"), "&lt;a&amp;&quot;b&apos;&gt;");
    }
}
