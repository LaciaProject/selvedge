//! R2: the server side of workerd's R2 binding protocol, and the operations the
//! REST API shares with it.
//!
//! workerd frames binding calls as HTTP:
//!
//! - `head`, `get`, `list`: `GET` with the request as JSON in `CF-R2-Request`.
//!   The response body is the metadata JSON (its length in `CF-R2-Metadata-Size`)
//!   followed by the object's bytes, for `get`.
//! - `put`, `delete` and the multipart calls: `PUT` whose body is the request
//!   JSON (its length in `CF-R2-Metadata-Size`) followed by the value. The
//!   response body is the result JSON.
//! - Errors: a status of 400 or more and `CF-R2-Error: {"version":0,"v4code":N,"message":...}`.
//!
//! The request JSON is Cap'n Proto's JSON encoding of `R2BindingRequest`
//! (workerd `src/workerd/api/r2-api.capnp`), where 64-bit integers may arrive as
//! strings.
//!
//! Metadata lives in PostgreSQL; bytes live in the object store under paths that
//! are never reused, so replacing an object swaps metadata in one transaction.

use std::collections::BTreeSet;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt, TryStreamExt};
use md5::{Digest, Md5};
use selvedge_store::r2::{PutOutcome, R2Object, R2Part, R2Upload, R2UploadedPart};
use selvedge_store::{Blobs, Db};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;

pub use selvedge_workerd::render::R2_BUCKET_HEADER as BUCKET_HEADER;
pub const MAX_KEY_BYTES: usize = 1024;
pub const MAX_PUT_BYTES: u64 = 5 * 1024 * 1024 * 1024 - 5 * 1024 * 1024;
pub const MIN_PART_BYTES: i64 = 5 * 1024 * 1024;
pub const MAX_LIST: usize = 1000;

/// An R2 error, as workerd expects it.
#[derive(Debug)]
pub struct R2Error {
    pub status: StatusCode,
    pub code: u32,
    pub message: String,
}

impl R2Error {
    pub fn new(status: StatusCode, code: u32, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }
    pub fn no_such_key() -> Self {
        Self::new(StatusCode::NOT_FOUND, 10007, "The specified key does not exist.")
    }
    pub fn precondition() -> Self {
        Self::new(
            StatusCode::PRECONDITION_FAILED,
            10031,
            "At least one of the pre-conditions you specified did not hold.",
        )
    }
    pub fn no_such_upload() -> Self {
        Self::new(StatusCode::NOT_FOUND, 10024, "The specified multipart upload does not exist.")
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, 10040, message)
    }
    fn header(&self) -> HeaderValue {
        let v = json!({ "version": 0, "v4code": self.code, "message": self.message }).to_string();
        HeaderValue::from_str(&v).unwrap_or_else(|_| HeaderValue::from_static("{}"))
    }
}

impl From<anyhow::Error> for R2Error {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %format!("{e:#}"), "R2 storage error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            10001,
            "We encountered an internal error. Please try again.",
        )
    }
}

impl IntoResponse for R2Error {
    fn into_response(self) -> Response {
        let mut r = (self.status, self.message.clone()).into_response();
        r.headers_mut().insert("cf-r2-error", self.header());
        r
    }
}

type R2Result<T> = Result<T, R2Error>;

// ---- JSON helpers ----

/// A 64-bit integer that Cap'n Proto JSON may encode as a string. The schema's
/// "unset" default (all ones) reads as `None`.
fn u64_field(v: &Value, name: &str) -> Option<u64> {
    let n = match v.get(name)? {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    (n != u64::MAX).then_some(n)
}

fn str_field<'a>(v: &'a Value, name: &str) -> Option<&'a str> {
    v.get(name).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub fn validate_key(key: &str) -> R2Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(R2Error::new(StatusCode::BAD_REQUEST, 10020, "The specified object name is not valid."));
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// The object as `R2HeadResponse` JSON.
pub fn object_json(o: &R2Object, http: bool, custom: bool) -> Value {
    let mut v = json!({
        "name": o.key,
        "version": o.version,
        "size": o.size,
        "etag": o.etag,
        "uploaded": o.uploaded_ms,
        "checksums": o.checksums,
        "storageClass": o.storage_class,
    });
    if http {
        v["httpFields"] = o.http_fields.clone();
    }
    if custom {
        v["customFields"] = o.custom_fields.clone();
    }
    v
}

// ---- conditions ----

fn etag_matches(list: &Value, etag: &str) -> bool {
    list.as_array().is_some_and(|l| {
        l.iter().any(|e| {
            e.get("type").and_then(Value::as_str) == Some("wildcard")
                || e.get("value").and_then(Value::as_str).map(|v| v.trim_matches('"')) == Some(etag)
        })
    })
}

fn has_entries(v: Option<&Value>) -> bool {
    v.and_then(Value::as_array).is_some_and(|a| !a.is_empty())
}

/// Whether `onlyIf` holds for `object` (`None`: the key is absent).
pub fn condition_holds(only_if: Option<&Value>, object: Option<&R2Object>) -> bool {
    let Some(c) = only_if.filter(|c| c.is_object()) else { return true };
    let Some(o) = object else {
        // Nothing to match: an If-Match fails, everything else holds.
        return !has_entries(c.get("etagMatches"));
    };
    if has_entries(c.get("etagMatches")) && !etag_matches(&c["etagMatches"], &o.etag) {
        return false;
    }
    if has_entries(c.get("etagDoesNotMatch")) && etag_matches(&c["etagDoesNotMatch"], &o.etag) {
        return false;
    }
    let seconds = c.get("secondsGranularity").and_then(Value::as_bool).unwrap_or(false);
    let at = |ms: i64| if seconds { ms / 1000 * 1000 } else { ms };
    let uploaded = at(o.uploaded_ms);
    if let Some(before) = u64_field(c, "uploadedBefore") {
        if uploaded >= at(before as i64) {
            return false;
        }
    }
    if let Some(after) = u64_field(c, "uploadedAfter") {
        if uploaded <= at(after as i64) {
            return false;
        }
    }
    true
}

// ---- ranges ----

/// Resolves a request range against an object's size to `(offset, length)`.
pub fn resolve_range(range: Option<&Value>, header: Option<&str>, size: u64) -> R2Result<Option<(u64, u64)>> {
    let invalid =
        || R2Error::new(StatusCode::RANGE_NOT_SATISFIABLE, 10039, "The requested range is not satisfiable.");
    if let Some(h) = header.filter(|h| !h.is_empty()) {
        let spec = h.trim().strip_prefix("bytes=").ok_or_else(invalid)?;
        let first = spec.split(',').next().unwrap_or_default().trim();
        let (a, b) = first.split_once('-').ok_or_else(invalid)?;
        return match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
            (Some(start), end) if start < size || size == 0 => {
                let end = end.map_or(size, |e| (e + 1).min(size));
                Ok(Some((start, end.saturating_sub(start))))
            }
            (None, Some(suffix)) => {
                let len = suffix.min(size);
                Ok(Some((size - len, len)))
            }
            _ => Err(invalid()),
        };
    }
    let Some(r) = range.filter(|r| r.is_object()) else { return Ok(None) };
    if let Some(suffix) = u64_field(r, "suffix") {
        let len = suffix.min(size);
        return Ok(Some((size - len, len)));
    }
    let offset = u64_field(r, "offset");
    let length = u64_field(r, "length");
    if offset.is_none() && length.is_none() {
        return Ok(None);
    }
    let offset = offset.unwrap_or(0);
    if offset > size {
        return Err(invalid());
    }
    let length = length.unwrap_or(size - offset).min(size - offset);
    Ok(Some((offset, length)))
}

/// Streams `start..start+len` of an object made of `parts`.
pub fn read_object(
    blobs: &Blobs,
    parts: &[R2Part],
    start: u64,
    len: u64,
) -> futures::stream::BoxStream<'static, std::io::Result<Bytes>> {
    let mut pieces = Vec::new();
    let (mut pos, end) = (0u64, start + len);
    for p in parts {
        let (a, b) = (pos, pos + p.size as u64);
        pos = b;
        if b <= start || a >= end {
            continue;
        }
        pieces.push((p.path.clone(), start.max(a) - a, end.min(b) - a));
    }
    let blobs = blobs.clone();
    futures::stream::iter(pieces)
        .then(move |(path, a, b)| {
            let blobs = blobs.clone();
            async move { blobs.read_range(&path, a, b).await }
        })
        .map_ok(|s| s.map_err(std::io::Error::other))
        .map_err(std::io::Error::other)
        .try_flatten()
        .boxed()
}

// ---- writes ----

/// Hashes requested by a put, to check against what arrives.
#[derive(Default)]
struct Expected {
    md5: Option<Vec<u8>>,
    sha1: Option<Vec<u8>>,
    sha256: Option<Vec<u8>>,
    sha384: Option<Vec<u8>>,
    sha512: Option<Vec<u8>>,
}

impl Expected {
    fn from_request(req: &Value) -> R2Result<Self> {
        use base64::Engine;
        let hexf = |name: &str| -> R2Result<Option<Vec<u8>>> {
            str_field(req, name)
                .map(|h| hex::decode(h).map_err(|_| R2Error::bad(format!("invalid {name}"))))
                .transpose()
        };
        Ok(Self {
            md5: str_field(req, "md5")
                .map(|b| {
                    base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .map_err(|_| R2Error::bad("invalid md5"))
                })
                .transpose()?,
            sha1: hexf("sha1")?,
            sha256: hexf("sha256")?,
            sha384: hexf("sha384")?,
            sha512: hexf("sha512")?,
        })
    }
}

/// What a written blob turned out to be.
pub struct Written {
    pub path: String,
    pub size: i64,
    pub md5: Vec<u8>,
    pub checksums: Value,
}

/// Writes `body` to a new blob at `path`, hashing as it goes.
async fn write_blob(
    blobs: &Blobs,
    path: String,
    body: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
    expected: &Expected,
) -> R2Result<Written> {
    use sha2::Digest as _;
    let mut writer = blobs.writer(&path);
    let mut md5 = Md5::new();
    let mut sha1 = expected.sha1.as_ref().map(|_| sha1::Sha1::new());
    let mut sha256 = expected.sha256.as_ref().map(|_| sha2::Sha256::new());
    let mut sha384 = expected.sha384.as_ref().map(|_| sha2::Sha384::new());
    let mut sha512 = expected.sha512.as_ref().map(|_| sha2::Sha512::new());
    let mut size = 0u64;
    let mut body = body;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| R2Error::bad(format!("reading the request body: {e}")))?;
        size += chunk.len() as u64;
        if size > MAX_PUT_BYTES {
            let _ = writer.abort().await;
            return Err(R2Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                100100,
                "Your proposed upload exceeds the maximum allowed object size.",
            ));
        }
        md5.update(&chunk);
        if let Some(h) = sha1.as_mut() {
            h.update(&chunk);
        }
        if let Some(h) = sha256.as_mut() {
            h.update(&chunk);
        }
        if let Some(h) = sha384.as_mut() {
            h.update(&chunk);
        }
        if let Some(h) = sha512.as_mut() {
            h.update(&chunk);
        }
        writer.write_all(&chunk).await.map_err(|e| R2Error::from(anyhow::Error::from(e)))?;
    }
    let md5 = md5.finalize().to_vec();
    let mut checksums = Map::new();
    checksums.insert("0".into(), json!(hex::encode(&md5)));
    let mut mismatch = expected.md5.as_ref().is_some_and(|e| *e != md5);
    for (slot, hasher, want) in [
        ("1", sha1.map(|h| h.finalize().to_vec()), &expected.sha1),
        ("2", sha256.map(|h| h.finalize().to_vec()), &expected.sha256),
        ("3", sha384.map(|h| h.finalize().to_vec()), &expected.sha384),
        ("4", sha512.map(|h| h.finalize().to_vec()), &expected.sha512),
    ] {
        if let (Some(got), Some(want)) = (hasher, want) {
            mismatch |= got != *want;
            checksums.insert(slot.into(), json!(hex::encode(got)));
        }
    }
    if mismatch {
        let _ = writer.abort().await;
        return Err(R2Error::new(
            StatusCode::BAD_REQUEST,
            10037,
            "The checksum you specified did not match what we received.",
        ));
    }
    writer.shutdown().await.map_err(|e| R2Error::from(anyhow::Error::from(e)))?;
    Ok(Written { path, size: size as i64, md5, checksums: Value::Object(checksums) })
}

/// The shared R2 operations over one bucket.
pub struct Bucket<'a> {
    pub db: &'a Db,
    pub blobs: &'a Blobs,
    pub id: String,
}

/// What wrote an object, as event notifications name it.
pub enum Change<'a> {
    Put,
    /// A copy from `bucket`/`key` (bucket by name).
    Copy {
        bucket: &'a str,
        key: &'a str,
    },
}

/// One object change, for event notifications.
struct Event {
    action: &'static str,
    key: String,
    /// `(size, etag)` of the new object.
    object: Option<(i64, String)>,
    copy_source: Option<(String, String)>,
}

pub enum GetResult {
    NotFound,
    PreconditionFailed(R2Object),
    Found(R2Object, Option<(u64, u64)>),
}

impl Bucket<'_> {
    fn new_blob_path(&self) -> String {
        format!("r2/{}/{}", self.id, selvedge_core::HexId::random())
    }

    pub async fn head(&self, key: &str) -> R2Result<Option<R2Object>> {
        validate_key(key)?;
        Ok(self.db.r2_head(&self.id, key).await?)
    }

    pub async fn get(&self, key: &str, req: &Value) -> R2Result<GetResult> {
        let Some(o) = self.head(key).await? else { return Ok(GetResult::NotFound) };
        if !condition_holds(req.get("onlyIf"), Some(&o)) {
            return Ok(GetResult::PreconditionFailed(o));
        }
        let range = resolve_range(req.get("range"), str_field(req, "rangeHeader"), o.size as u64)?;
        Ok(GetResult::Found(o, range))
    }

    /// Stores a value. `Ok(None)`: a condition in `req.onlyIf` failed.
    pub async fn put(
        &self,
        key: &str,
        req: &Value,
        body: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
    ) -> R2Result<Option<R2Object>> {
        self.put_as(key, req, body, Change::Put).await
    }

    /// [`Bucket::put`], reporting the write to event notifications as `change`.
    pub async fn put_as(
        &self,
        key: &str,
        req: &Value,
        body: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
        change: Change<'_>,
    ) -> R2Result<Option<R2Object>> {
        validate_key(key)?;
        let expected = Expected::from_request(req)?;
        // Fail early on a condition that already does not hold; it is checked
        // again atomically before the object is replaced.
        let current = self.db.r2_head(&self.id, key).await?;
        if !condition_holds(req.get("onlyIf"), current.as_ref()) {
            return Ok(None);
        }
        let written = write_blob(self.blobs, self.new_blob_path(), body, &expected).await?;
        let object = R2Object {
            key: key.to_owned(),
            version: selvedge_core::HexId::random().to_string(),
            size: written.size,
            etag: hex::encode(&written.md5),
            uploaded_ms: now_ms(),
            http_fields: req.get("httpFields").cloned().filter(Value::is_object).unwrap_or_else(|| json!({})),
            custom_fields: req
                .get("customFields")
                .cloned()
                .filter(Value::is_array)
                .unwrap_or_else(|| json!([])),
            checksums: written.checksums,
            storage_class: str_field(req, "storageClass").unwrap_or("Standard").to_owned(),
            parts: vec![R2Part { path: written.path, size: written.size }],
        };
        let only_if = req.get("onlyIf").cloned();
        match self.db.r2_put(&self.id, &object, |cur| condition_holds(only_if.as_ref(), cur)).await? {
            PutOutcome::Stored => {
                let (action, copy_source) = match change {
                    Change::Put => ("PutObject", None),
                    Change::Copy { bucket, key } => ("CopyObject", Some((bucket.to_owned(), key.to_owned()))),
                };
                self.notify(vec![Event {
                    action,
                    key: key.to_owned(),
                    object: Some((object.size, object.etag.clone())),
                    copy_source,
                }])
                .await;
                Ok(Some(object))
            }
            PutOutcome::PreconditionFailed => Ok(None),
        }
    }

    /// Sends object changes to the queues of matching notification rules.
    /// A failure here is logged; the change itself has happened.
    async fn notify(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let targets = match self.db.r2_notification_targets(&self.id).await {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(bucket = %self.id, error = %format!("{e:#}"), "reading R2 notification rules failed");
                return;
            }
        };
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for queue in &targets.queues {
            let messages: Vec<selvedge_store::queues::NewMessage> = events
                .iter()
                .filter(|e| queue.rules.iter().any(|r| r.matches(e.action, &e.key)))
                .map(|e| {
                    let mut object = json!({ "key": e.key });
                    if let Some((size, etag)) = &e.object {
                        object["size"] = json!(size);
                        object["eTag"] = json!(etag);
                    }
                    let mut m = json!({
                        "account": targets.account_id,
                        "action": e.action,
                        "bucket": targets.bucket_name,
                        "object": object,
                        "eventTime": now,
                    });
                    if let Some((bucket, key)) = &e.copy_source {
                        m["copySource"] = json!({ "bucket": bucket, "object": key });
                    }
                    selvedge_store::queues::NewMessage {
                        body: m.to_string().into_bytes(),
                        content_type: "json".into(),
                        delay_secs: 0,
                    }
                })
                .collect();
            if messages.is_empty() {
                continue;
            }
            if let Err(e) = self.db.enqueue(&queue.queue_id, &messages).await {
                tracing::warn!(queue = %queue.queue_id, error = %format!("{e:#}"), "sending R2 notifications failed");
            }
        }
    }

    pub async fn delete(&self, keys: Vec<String>) -> R2Result<()> {
        for k in &keys {
            validate_key(k)?;
        }
        if keys.len() > 1000 {
            return Err(R2Error::bad("You can delete at most 1000 objects at once."));
        }
        let deleted = self.db.r2_delete(&self.id, &keys).await?;
        self.notify(
            deleted
                .into_iter()
                .map(|key| Event { action: "DeleteObject", key, object: None, copy_source: None })
                .collect(),
        )
        .await;
        Ok(())
    }

    pub async fn list(&self, req: &Value) -> R2Result<Value> {
        let limit = u64_field(req, "limit").map_or(MAX_LIST, |l| (l as usize).clamp(1, MAX_LIST));
        let prefix = str_field(req, "prefix").unwrap_or("");
        let delimiter = str_field(req, "delimiter");
        let cursor =
            str_field(req, "cursor").map(crate::kvlogic::decode_cursor).transpose().map_err(R2Error::bad)?;
        let start_after = str_field(req, "startAfter").unwrap_or("");
        let include: Vec<u64> = req
            .get("include")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                    .collect()
            })
            .unwrap_or_default();
        let (http, custom) = (include.contains(&0), include.contains(&1));

        let mut after = cursor.unwrap_or_default().max(start_after.to_owned());
        let mut objects = Vec::new();
        let mut prefixes = BTreeSet::new();
        let mut skip: Option<String> = None;
        let mut truncated = false;
        'outer: loop {
            let batch = self.db.r2_list(&self.id, prefix, &after, 1000).await?;
            if batch.is_empty() {
                break;
            }
            for o in batch {
                if objects.len() + prefixes.len() >= limit {
                    truncated = true;
                    break 'outer;
                }
                after = o.key.clone();
                if skip.as_deref().is_some_and(|s| o.key.starts_with(s)) {
                    continue;
                }
                if let Some(d) = delimiter {
                    if let Some(pos) = o.key[prefix.len()..].find(d) {
                        let p = o.key[..prefix.len() + pos + d.len()].to_owned();
                        skip = Some(p.clone());
                        prefixes.insert(p);
                        continue;
                    }
                }
                objects.push(object_json(&o, http, custom));
            }
        }
        let mut out = json!({
            "objects": objects,
            "truncated": truncated,
            "delimitedPrefixes": prefixes.into_iter().collect::<Vec<_>>(),
        });
        if truncated {
            out["cursor"] = json!(crate::kvlogic::encode_cursor(&after));
        }
        Ok(out)
    }

    pub async fn create_upload(&self, key: &str, req: &Value) -> R2Result<String> {
        validate_key(key)?;
        let id = selvedge_core::HexId::random().to_string();
        let upload = R2Upload {
            id: id.clone(),
            key: key.to_owned(),
            http_fields: req.get("httpFields").cloned().filter(Value::is_object).unwrap_or_else(|| json!({})),
            custom_fields: req
                .get("customFields")
                .cloned()
                .filter(Value::is_array)
                .unwrap_or_else(|| json!([])),
            storage_class: str_field(req, "storageClass").unwrap_or("Standard").to_owned(),
        };
        self.db.r2_create_upload(&self.id, &upload).await?;
        Ok(id)
    }

    pub async fn upload_part(
        &self,
        key: &str,
        upload: &str,
        part: u64,
        body: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
    ) -> R2Result<String> {
        if !(1..=10_000).contains(&part) {
            return Err(R2Error::bad("Part number must be between 1 and 10000."));
        }
        let u = self.db.r2_upload(&self.id, upload).await?.ok_or_else(R2Error::no_such_upload)?;
        if u.key != key {
            return Err(R2Error::no_such_upload());
        }
        let path = format!("r2/{}/mpu/{upload}/{part}-{}", self.id, selvedge_core::HexId::random());
        let written = write_blob(self.blobs, path, body, &Expected::default()).await?;
        let etag = hex::encode(&written.md5);
        let recorded = self
            .db
            .r2_put_part(
                upload,
                &R2UploadedPart {
                    part: part as i32,
                    etag: etag.clone(),
                    size: written.size,
                    path: written.path,
                },
            )
            .await?;
        if !recorded {
            return Err(R2Error::no_such_upload());
        }
        Ok(etag)
    }

    pub async fn complete_upload(&self, key: &str, upload: &str, parts: &Value) -> R2Result<R2Object> {
        let u = self.db.r2_upload(&self.id, upload).await?.ok_or_else(R2Error::no_such_upload)?;
        if u.key != key {
            return Err(R2Error::no_such_upload());
        }
        let stored = self.db.r2_upload_parts(upload).await?;
        let requested = parts.as_array().cloned().unwrap_or_default();
        if requested.is_empty() {
            return Err(R2Error::new(StatusCode::BAD_REQUEST, 10025, "There must be at least one part."));
        }
        let invalid_part = || {
            R2Error::new(
                StatusCode::BAD_REQUEST,
                10025,
                "One or more of the specified parts could not be found.",
            )
        };
        let mut chosen = Vec::new();
        let mut last = 0u64;
        for p in &requested {
            let number = u64_field(p, "part").ok_or_else(invalid_part)?;
            if number <= last {
                return Err(R2Error::new(
                    StatusCode::BAD_REQUEST,
                    10025,
                    "Parts must be in ascending order.",
                ));
            }
            last = number;
            let etag = str_field(p, "etag").unwrap_or("").trim_matches('"');
            let s =
                stored.iter().find(|s| s.part as u64 == number && s.etag == etag).ok_or_else(invalid_part)?;
            chosen.push(s.clone());
        }
        if chosen[..chosen.len() - 1].iter().any(|p| p.size < MIN_PART_BYTES) {
            return Err(R2Error::new(
                StatusCode::BAD_REQUEST,
                10011,
                "Your proposed upload is smaller than the minimum allowed object size.",
            ));
        }
        let mut md5 = Md5::new();
        for p in &chosen {
            md5.update(hex::decode(&p.etag).unwrap_or_default());
        }
        let object = R2Object {
            key: key.to_owned(),
            version: selvedge_core::HexId::random().to_string(),
            size: chosen.iter().map(|p| p.size).sum(),
            etag: format!("{}-{}", hex::encode(md5.finalize()), chosen.len()),
            uploaded_ms: now_ms(),
            http_fields: u.http_fields,
            custom_fields: u.custom_fields,
            checksums: json!({}),
            storage_class: u.storage_class,
            parts: chosen.iter().map(|p| R2Part { path: p.path.clone(), size: p.size }).collect(),
        };
        if !self.db.r2_complete_upload(&self.id, upload, &object).await? {
            return Err(R2Error::no_such_upload());
        }
        self.notify(vec![Event {
            action: "CompleteMultipartUpload",
            key: key.to_owned(),
            object: Some((object.size, object.etag.clone())),
            copy_source: None,
        }])
        .await;
        Ok(object)
    }

    pub async fn abort_upload(&self, key: &str, upload: &str) -> R2Result<()> {
        if let Some(u) = self.db.r2_upload(&self.id, upload).await? {
            if u.key == key {
                self.db.r2_abort_upload(upload).await?;
            }
        }
        Ok(())
    }
}

// ---- the binding protocol ----

fn metadata_response(
    status: StatusCode,
    metadata: Value,
    body: Option<Body>,
    extra: Option<R2Error>,
) -> Response {
    let meta = metadata.to_string();
    let len = meta.len();
    let body = match body {
        None => Body::from(meta),
        Some(b) => {
            let head = futures::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(meta)) });
            Body::from_stream(head.chain(b.into_data_stream().map_err(std::io::Error::other)))
        }
    };
    let mut r = Response::new(body);
    *r.status_mut() = status;
    r.headers_mut().insert("cf-r2-metadata-size", HeaderValue::from(len));
    if let Some(e) = extra {
        r.headers_mut().insert("cf-r2-error", e.header());
    }
    r
}

/// Handles a binding `GET` (head, get, list).
pub async fn binding_get(bucket: &Bucket<'_>, headers: &HeaderMap) -> Response {
    let req: Value =
        match headers.get("cf-r2-request").and_then(|v| v.to_str().ok()).map(serde_json::from_str) {
            Some(Ok(v)) => v,
            _ => return R2Error::bad("missing or invalid CF-R2-Request").into_response(),
        };
    let result: R2Result<Response> = async {
        match req.get("method").and_then(Value::as_str) {
            Some("head") => {
                let key = str_field(&req, "object").unwrap_or("");
                let o = bucket.head(key).await?.ok_or_else(R2Error::no_such_key)?;
                Ok(metadata_response(StatusCode::OK, object_json(&o, true, true), None, None))
            }
            Some("get") => {
                let key = str_field(&req, "object").unwrap_or("");
                match bucket.get(key, &req).await? {
                    GetResult::NotFound => Err(R2Error::no_such_key()),
                    GetResult::PreconditionFailed(o) => Ok(metadata_response(
                        StatusCode::PRECONDITION_FAILED,
                        object_json(&o, true, true),
                        None,
                        Some(R2Error::precondition()),
                    )),
                    GetResult::Found(o, range) => {
                        let (start, len) = range.unwrap_or((0, o.size as u64));
                        let mut meta = object_json(&o, true, true);
                        if range.is_some() {
                            meta["range"] = json!({ "offset": start, "length": len });
                        }
                        let stream = read_object(bucket.blobs, &o.parts, start, len);
                        Ok(metadata_response(StatusCode::OK, meta, Some(Body::from_stream(stream)), None))
                    }
                }
            }
            Some("list") => Ok(metadata_response(StatusCode::OK, bucket.list(&req).await?, None, None)),
            other => Err(R2Error::bad(format!("unsupported R2 method {other:?}"))),
        }
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// Splits a binding `PUT` body into its request JSON and the value stream.
async fn split_put(
    headers: &HeaderMap,
    body: Body,
) -> R2Result<(Value, impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin)> {
    let size: usize = headers
        .get("cf-r2-metadata-size")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .filter(|n| *n <= 1 << 20)
        .ok_or_else(|| R2Error::bad("missing or invalid CF-R2-Metadata-Size"))?;
    let mut stream = body.into_data_stream();
    let mut head = Vec::with_capacity(size);
    let mut rest = None;
    while head.len() < size {
        let chunk = stream
            .next()
            .await
            .ok_or_else(|| R2Error::bad("request body shorter than its metadata"))?
            .map_err(|e| R2Error::bad(e.to_string()))?;
        let need = size - head.len();
        if chunk.len() > need {
            head.extend_from_slice(&chunk[..need]);
            rest = Some(chunk.slice(need..));
        } else {
            head.extend_from_slice(&chunk);
        }
    }
    let req: Value =
        serde_json::from_slice(&head).map_err(|e| R2Error::bad(format!("invalid request JSON: {e}")))?;
    let first = futures::stream::iter(rest.map(Ok));
    Ok((req, first.chain(stream).boxed()))
}

/// Handles a binding `PUT` (put, delete, multipart).
pub async fn binding_put(bucket: &Bucket<'_>, headers: &HeaderMap, body: Body) -> Response {
    let result: R2Result<Value> = async {
        let (req, value) = split_put(headers, body).await?;
        let key = str_field(&req, "object").unwrap_or("").to_owned();
        match req.get("method").and_then(Value::as_str) {
            Some("put") => match bucket.put(&key, &req, value).await? {
                Some(o) => Ok(object_json(&o, true, true)),
                None => Err(R2Error::precondition()),
            },
            Some("delete") => {
                let keys = match req.get("objects").and_then(Value::as_array) {
                    Some(list) => list.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                    None => vec![key],
                };
                bucket.delete(keys).await?;
                Ok(json!({}))
            }
            Some("createMultipartUpload") => {
                Ok(json!({ "uploadId": bucket.create_upload(&key, &req).await? }))
            }
            Some("uploadPart") => {
                let upload = str_field(&req, "uploadId").unwrap_or("");
                let part = u64_field(&req, "partNumber").unwrap_or(0);
                Ok(json!({ "etag": bucket.upload_part(&key, upload, part, value).await? }))
            }
            Some("completeMultipartUpload") => {
                let upload = str_field(&req, "uploadId").unwrap_or("");
                let o =
                    bucket.complete_upload(&key, upload, req.get("parts").unwrap_or(&Value::Null)).await?;
                Ok(object_json(&o, true, true))
            }
            Some("abortMultipartUpload") => {
                bucket.abort_upload(&key, str_field(&req, "uploadId").unwrap_or("")).await?;
                Ok(json!({}))
            }
            other => Err(R2Error::bad(format!("unsupported R2 method {other:?}"))),
        }
    }
    .await;
    match result {
        Ok(v) => (StatusCode::OK, [("content-type", "application/json")], v.to_string()).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Deletes blobs queued at least `grace` ago, in batches.
pub async fn collect_garbage(db: &Db, blobs: &Blobs, grace_secs: i64) -> Result<usize> {
    let mut total = 0;
    loop {
        let due = db.garbage_due(grace_secs, 500).await?;
        if due.is_empty() {
            return Ok(total);
        }
        for p in &due {
            blobs.delete(p).await?;
        }
        db.garbage_done(&due).await?;
        total += due.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(etag: &str, uploaded_ms: i64) -> R2Object {
        R2Object {
            key: "k".into(),
            version: "v".into(),
            size: 10,
            etag: etag.into(),
            uploaded_ms,
            http_fields: json!({}),
            custom_fields: json!([]),
            checksums: json!({}),
            storage_class: "Standard".into(),
            parts: vec![],
        }
    }

    #[test]
    fn conditions() {
        let o = obj("abc", 5_000);
        assert!(condition_holds(None, Some(&o)));
        let m = json!({"etagMatches": [{"value": "abc", "type": "strong"}]});
        assert!(condition_holds(Some(&m), Some(&o)));
        assert!(!condition_holds(Some(&m), None));
        let nm = json!({"etagDoesNotMatch": [{"value": "", "type": "wildcard"}]});
        assert!(!condition_holds(Some(&nm), Some(&o)));
        assert!(condition_holds(Some(&nm), None));
        let before = json!({"uploadedBefore": "4000"});
        assert!(!condition_holds(Some(&before), Some(&o)));
        let unset = json!({"uploadedBefore": "18446744073709551615"});
        assert!(condition_holds(Some(&unset), Some(&o)));
    }

    #[test]
    fn ranges() {
        assert_eq!(resolve_range(None, None, 10).unwrap(), None);
        assert_eq!(
            resolve_range(Some(&json!({"offset": "2", "length": "3"})), None, 10).unwrap(),
            Some((2, 3))
        );
        assert_eq!(resolve_range(Some(&json!({"offset": 8})), None, 10).unwrap(), Some((8, 2)));
        assert_eq!(resolve_range(Some(&json!({"suffix": 4})), None, 10).unwrap(), Some((6, 4)));
        assert_eq!(resolve_range(None, Some("bytes=1-3"), 10).unwrap(), Some((1, 3)));
        assert_eq!(resolve_range(None, Some("bytes=-2"), 10).unwrap(), Some((8, 2)));
        assert!(resolve_range(Some(&json!({"offset": 11})), None, 10).is_err());
    }
}
