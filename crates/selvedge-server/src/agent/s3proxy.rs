//! A per-agent S3 proxy for Durable Object storage, so that no workerd process
//! holds the object store's credentials.
//!
//! Each workerd process that joins a cluster gets its own access key from
//! [`S3Proxy::register`], valid only here and only until
//! [`S3Proxy::retire`]. The proxy checks the request's SigV4 signature with
//! that key's secret, allows only the account's own prefix
//! (`<prefix>/<account>/...`) and the operations workerd's cluster mode uses,
//! then signs the request again with the real credentials and forwards it. A
//! compromised process can therefore read and write only its own account's
//! Durable Objects, whatever the object store supports in the way of scoped
//! credentials.
//!
//! A key per process also tells the proxy which cluster member a process is:
//! it notes the lease record (`v1/nodes/<node>.json`) each one writes. When a
//! process ends without leaving the cluster (a crash, a kill), `retire`
//! expires that record, as workerd does itself when it stops cleanly, so that
//! peers take its Durable Objects over at once instead of after the lease
//! (20 seconds).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use percent_encoding::{percent_decode_str, utf8_percent_encode};
use selvedge_core::AccountId;

use crate::proxy::{self, Body};
use crate::sigv4::{AWS_PATH, amz_date_secs, parse_authorization, signature};

/// The largest object the proxy carries. Durable Object storage writes are
/// far smaller.
const MAX_BODY: usize = 256 << 20;
/// How far a request's `x-amz-date` may be from now, as S3 allows.
const MAX_SKEW_SECS: i64 = 15 * 60;
/// The proxy's own bound on a request to the store.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);
/// How many times `retire` rereads a lease record that changed under it.
const EXPIRE_ATTEMPTS: usize = 5;

/// The real object store.
#[derive(Clone, Debug)]
pub struct Upstream {
    /// `scheme://host[:port]`, without a path.
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// A workerd process's storage credentials.
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

/// What `retire` did about the process's lease record.
#[derive(Debug, PartialEq, Eq)]
pub enum Retired {
    /// The process never wrote one: it had not joined the cluster.
    NoRecord,
    /// The process had already left the cluster, or another process wrote
    /// the record since.
    AlreadyGone,
    /// The record was expired: peers take the process's objects over now.
    Expired { node: String },
}

/// A lease record a process wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
struct NodeRecord {
    /// Its key in the bucket.
    key: String,
    node: String,
    /// workerd's random id for the process that wrote it.
    instance: u64,
}

/// A registered workerd process.
struct Process {
    account: AccountId,
    secret: String,
    /// Requests forwarded to the store and not yet answered.
    in_flight: tokio::sync::watch::Sender<usize>,
    /// The last lease record it wrote.
    node: Mutex<Option<NodeRecord>>,
}

/// Counts a request in flight for as long as it lives.
struct InFlight(Arc<Process>);

impl InFlight {
    fn new(p: Arc<Process>) -> Self {
        p.in_flight.send_modify(|n| *n += 1);
        Self(p)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.send_modify(|n| *n -= 1);
    }
}

pub struct S3Proxy {
    upstream: Upstream,
    bucket: String,
    /// The cluster prefix inside the bucket, without slashes at either end.
    prefix: String,
    /// Registered processes, by access key id.
    processes: Mutex<HashMap<String, Arc<Process>>>,
    http: reqwest::Client,
}

/// Splits `s3://bucket/prefix` into the bucket and the prefix.
pub fn split_bucket_url(url: &str) -> Result<(String, String)> {
    let rest = url.strip_prefix("s3://").context("the Durable Object bucket must be an s3:// URL")?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    anyhow::ensure!(!bucket.is_empty(), "the Durable Object bucket URL has no bucket");
    Ok((bucket.to_owned(), prefix.trim_matches('/').to_owned()))
}

/// The lease record a successful PUT of `body` to `key` (under the account's
/// prefix `own`) wrote, if it was one: `v1/nodes/<node>.json` holding a JSON
/// object that names `<node>` and the writing process's `instance`, with a
/// lease that has not been given up (`expires_ms` above 0). Anything else,
/// including a layout a later workerd may use, is not tracked, and its
/// process's objects wait for the lease as before.
fn lease_record(own: &str, key: &str, body: &[u8]) -> Option<NodeRecord> {
    let node = key.strip_prefix(own)?.strip_prefix("v1/nodes/")?.strip_suffix(".json")?;
    if node.is_empty() || node.contains('/') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if v.get("node")?.as_str()? != node || v.get("expires_ms")?.as_u64()? == 0 {
        return None;
    }
    Some(NodeRecord { key: key.to_owned(), node: node.to_owned(), instance: v.get("instance")?.as_u64()? })
}

/// `record` with its lease given up, as workerd writes it when it leaves the
/// cluster: `expires_ms` 0 and `draining`. Other fields are kept. `None` if it
/// is not the record `seen` (another process's), or is already given up.
fn expired(record: &[u8], seen: &NodeRecord) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(record).ok()?;
    let o = v.as_object_mut()?;
    if o.get("node")?.as_str()? != seen.node
        || o.get("instance")?.as_u64()? != seen.instance
        || o.get("expires_ms")?.as_u64()? == 0
    {
        return None;
    }
    o.insert("expires_ms".into(), 0.into());
    o.insert("draining".into(), true.into());
    serde_json::to_vec(&v).ok()
}

fn s3_error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    let mut r = proxy::full(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{message}</Message></Error>"
        ),
    );
    r.headers_mut().insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
    r
}

impl S3Proxy {
    pub fn new(upstream: Upstream, bucket_url: &str) -> Result<Arc<Self>> {
        let (bucket, prefix) = split_bucket_url(bucket_url)?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(UPSTREAM_TIMEOUT)
            .build()?;
        Ok(Arc::new(Self {
            upstream: Upstream { endpoint: upstream.endpoint.trim_end_matches('/').to_owned(), ..upstream },
            bucket,
            prefix,
            processes: Mutex::new(HashMap::new()),
            http,
        }))
    }

    /// The account's key prefix inside the bucket.
    fn account_prefix(&self, account: &AccountId) -> String {
        if self.prefix.is_empty() { format!("{account}/") } else { format!("{}/{account}/", self.prefix) }
    }

    /// New credentials for one workerd process of `account`, valid until
    /// [`retire`](Self::retire).
    pub fn register(&self, account: &AccountId) -> Credentials {
        let access_key_id = format!("{account}-{}", hex::encode(rand::random::<[u8; 8]>()));
        let secret_access_key = hex::encode(rand::random::<[u8; 32]>());
        let process = Process {
            account: account.clone(),
            secret: secret_access_key.clone(),
            in_flight: tokio::sync::watch::Sender::new(0),
            node: Mutex::new(None),
        };
        self.processes.lock().expect("poisoned").insert(access_key_id.clone(), Arc::new(process));
        Credentials { access_key_id, secret_access_key }
    }

    /// Ends a process's credentials, leaving its lease alone: for a process
    /// that may not have exited.
    pub fn revoke(&self, access_key_id: &str) {
        self.processes.lock().expect("poisoned").remove(access_key_id);
    }

    /// Ends the credentials of a process that has exited, and gives up its
    /// cluster lease, so that peers take its Durable Objects over at once.
    ///
    /// The caller must have seen the process exit: a live one would serve on
    /// while its peers take its objects. A request it left behind cannot land
    /// afterwards either: none starts once the credentials are gone, and this
    /// waits for those forwarded before to be answered. The record is replaced
    /// by a conditional write, as workerd does when it leaves, so a renewal
    /// the store had yet to apply cannot revive it.
    pub async fn retire(&self, access_key_id: &str) -> Result<Retired> {
        let Some(process) = self.processes.lock().expect("poisoned").remove(access_key_id) else {
            return Ok(Retired::NoRecord);
        };
        // The store answers each within UPSTREAM_TIMEOUT.
        let mut in_flight = process.in_flight.subscribe();
        tokio::time::timeout(UPSTREAM_TIMEOUT + Duration::from_secs(5), in_flight.wait_for(|n| *n == 0))
            .await
            .context("requests of the process were still in flight")??;
        let Some(seen) = process.node.lock().expect("poisoned").clone() else {
            return Ok(Retired::NoRecord);
        };
        let path = format!("/{}/{}", self.bucket, utf8_percent_encode(&seen.key, AWS_PATH));
        for _ in 0..EXPIRE_ATTEMPTS {
            let current = self.upstream_request(Method::GET, &path, "").send().await?;
            if current.status() == StatusCode::NOT_FOUND {
                return Ok(Retired::AlreadyGone);
            }
            anyhow::ensure!(current.status().is_success(), "reading the lease record: {}", current.status());
            let etag =
                current.headers().get(http::header::ETAG).cloned().context("the store sent no ETag")?;
            let Some(body) = expired(&current.bytes().await?, &seen) else {
                return Ok(Retired::AlreadyGone);
            };
            let put = self
                .upstream_request(Method::PUT, &path, "")
                .header(http::header::IF_MATCH, etag)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await?;
            if put.status().is_success() {
                return Ok(Retired::Expired { node: seen.node });
            }
            // Written since we read it: read it again.
            anyhow::ensure!(
                put.status() == StatusCode::PRECONDITION_FAILED,
                "expiring the lease record: {}",
                put.status()
            );
        }
        anyhow::bail!("the lease record kept changing")
    }

    /// Listens on `addr` (use port 0 for any) and returns the bound address.
    pub async fn serve(self: Arc<Self>, addr: SocketAddr) -> Result<SocketAddr> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let handler = Arc::new(move |req: Request<Incoming>| {
            let me = self.clone();
            async move { me.handle(req).await }
        });
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => proxy::serve_connection(stream, handler.clone()),
                    Err(e) => {
                        tracing::error!(error = %e, "S3 proxy accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });
        Ok(local)
    }

    async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<Body> {
        let (path, process) = match self.check(&req) {
            Ok(ok) => ok,
            Err((status, code, msg)) => {
                tracing::warn!(method = %req.method(), path = %req.uri().path(), %code, msg, "S3 proxy refused a request");
                return s3_error(status, code, msg);
            }
        };
        let (parts, body) = req.into_parts();
        let body = match http_body_util::Limited::new(body, MAX_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return s3_error(StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge", "object too large"),
        };
        // Counted until the store answers, and carried on if the client goes
        // away meanwhile, so that `retire` knows when nothing of a process's
        // can land any more.
        let in_flight = InFlight::new(process);
        let me = self.clone();
        let forwarded = tokio::spawn(async move {
            let response = me.forward(&parts, body.clone(), &path).await;
            if parts.method == Method::PUT && response.status().is_success() {
                me.note_lease(&in_flight.0, &path, &body);
            }
            drop(in_flight);
            response
        });
        forwarded.await.unwrap_or_else(|_| {
            s3_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", "the proxy failed the request")
        })
    }

    /// Notes the lease record a process wrote to `canonical_path`, if that
    /// is what it wrote.
    fn note_lease(&self, process: &Process, canonical_path: &str, body: &[u8]) {
        let Ok(decoded) = percent_decode_str(canonical_path).decode_utf8() else { return };
        let Some(key) = decoded.strip_prefix(&format!("/{}/", self.bucket)) else { return };
        if let Some(record) = lease_record(&self.account_prefix(&process.account), key, body) {
            *process.node.lock().expect("poisoned") = Some(record);
        }
    }

    /// Authenticates and authorizes a request. Returns its canonical path and
    /// the process that sent it.
    fn check(
        &self,
        req: &Request<Incoming>,
    ) -> Result<(String, Arc<Process>), (StatusCode, &'static str, &'static str)> {
        let denied = (StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
        let h = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
        let auth = h("authorization").and_then(parse_authorization).ok_or((
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "missing or malformed SigV4 authorization",
        ))?;
        let process = self.processes.lock().expect("poisoned").get(&auth.access_key_id).cloned().ok_or((
            StatusCode::FORBIDDEN,
            "InvalidAccessKeyId",
            "unknown or retired access key",
        ))?;
        let amz_date = h("x-amz-date").ok_or(denied)?;
        let now = chrono::Utc::now().timestamp();
        if amz_date_secs(amz_date).is_none_or(|t| (t - now).abs() > MAX_SKEW_SECS) {
            return Err((StatusCode::FORBIDDEN, "RequestTimeTooSkewed", "x-amz-date is too far from now"));
        }
        let payload_hash = h("x-amz-content-sha256").ok_or(denied)?;
        if !auth.signed_headers.iter().any(|s| s == "host") {
            return Err(denied);
        }
        let mut headers = Vec::new();
        for name in &auth.signed_headers {
            let v = h(name).ok_or(denied)?;
            headers.push((name.clone(), v.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        // The path as the client signed it: decoded, then encoded as SigV4 does.
        let raw_path = req.uri().path();
        let decoded = percent_decode_str(raw_path).decode_utf8().map_err(|_| denied)?.into_owned();
        let canonical_path = utf8_percent_encode(&decoded, AWS_PATH).to_string();
        let query = req.uri().query().unwrap_or("");
        let expected = signature(
            &process.secret,
            req.method().as_str(),
            &canonical_path,
            query,
            &headers,
            payload_hash,
            amz_date,
            &auth.region,
        );
        if !crate::constant_time_eq(expected.as_bytes(), auth.signature.as_bytes()) {
            return Err((StatusCode::FORBIDDEN, "SignatureDoesNotMatch", "signature does not match"));
        }

        // Authorization: the account's own prefix only.
        if decoded.split('/').any(|seg| seg == ".." || seg == ".") {
            return Err(denied);
        }
        let own = self.account_prefix(&process.account);
        let bucket_root = format!("/{}", self.bucket);
        let key = decoded.strip_prefix(&bucket_root).and_then(|k| k.strip_prefix('/'));
        match key {
            // Bucket-level: only a listing of the account's prefix.
            Some("") | None if decoded == bucket_root || decoded == format!("{bucket_root}/") => {
                let params: Vec<(String, String)> =
                    url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
                let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
                let ok = req.method() == Method::GET
                    && get("list-type") == Some("2")
                    && get("prefix").is_some_and(|p| p.starts_with(&own))
                    && params.iter().all(|(k, _)| {
                        [
                            "list-type",
                            "prefix",
                            "delimiter",
                            "continuation-token",
                            "start-after",
                            "max-keys",
                            "encoding-type",
                            "fetch-owner",
                        ]
                        .contains(&k.as_str())
                    });
                if ok { Ok((canonical_path, process)) } else { Err(denied) }
            }
            Some(k) if k.starts_with(&own) => {
                let allowed = [Method::GET, Method::HEAD, Method::PUT, Method::DELETE, Method::POST];
                if allowed.contains(req.method()) { Ok((canonical_path, process)) } else { Err(denied) }
            }
            _ => Err(denied),
        }
    }

    /// A request to the store, signed with the real credentials.
    fn upstream_request(&self, method: Method, canonical_path: &str, query: &str) -> reqwest::RequestBuilder {
        let url = if query.is_empty() {
            format!("{}{canonical_path}", self.upstream.endpoint)
        } else {
            format!("{}{canonical_path}?{query}", self.upstream.endpoint)
        };
        let host = self
            .upstream
            .endpoint
            .split_once("://")
            .map_or(self.upstream.endpoint.as_str(), |(_, h)| h)
            .to_owned();
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let payload_hash = "UNSIGNED-PAYLOAD";
        let mut signed = vec![
            ("host".to_owned(), host),
            ("x-amz-content-sha256".to_owned(), payload_hash.to_owned()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        if let Some(t) = &self.upstream.session_token {
            signed.push(("x-amz-security-token".to_owned(), t.clone()));
        }
        let sig = signature(
            &self.upstream.secret_access_key,
            method.as_str(),
            canonical_path,
            query,
            &signed,
            payload_hash,
            &amz_date,
            &self.upstream.region,
        );
        let names: Vec<&str> = signed.iter().map(|(k, _)| k.as_str()).collect();
        let mut out = self.http.request(method, url);
        for (k, v) in &signed[1..] {
            out = out.header(k.as_str(), v.as_str());
        }
        out.header(
            "authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{}/{}/s3/aws4_request, SignedHeaders={}, Signature={sig}",
                self.upstream.access_key_id,
                &amz_date[..8],
                self.upstream.region,
                names.join(";")
            ),
        )
    }

    async fn forward(
        &self,
        parts: &http::request::Parts,
        body: Bytes,
        canonical_path: &str,
    ) -> Response<Body> {
        let query = parts.uri.query().unwrap_or("");
        let mut out = self.upstream_request(parts.method.clone(), canonical_path, query).body(body);
        // Conditional writes and object metadata pass through unsigned, as
        // workerd sends them.
        for (name, value) in &parts.headers {
            let n = name.as_str();
            let pass = matches!(
                n,
                "if-match"
                    | "if-none-match"
                    | "if-modified-since"
                    | "if-unmodified-since"
                    | "range"
                    | "content-type"
                    | "content-md5"
                    | "content-encoding"
                    | "cache-control"
            ) || (n.starts_with("x-amz-")
                && !matches!(n, "x-amz-date" | "x-amz-content-sha256" | "x-amz-security-token"));
            if pass {
                out = out.header(name, value);
            }
        }
        let resp = match out.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "S3 proxy upstream request failed");
                return s3_error(StatusCode::BAD_GATEWAY, "ServiceUnavailable", "object store unreachable");
            }
        };
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "S3 proxy upstream response failed");
                return s3_error(
                    StatusCode::BAD_GATEWAY,
                    "ServiceUnavailable",
                    "object store response failed",
                );
            }
        };
        let mut r = Response::new(Full::new(bytes).map_err(|e| match e {}).boxed());
        *r.status_mut() = status;
        for (k, v) in &headers {
            if !matches!(k.as_str(), "transfer-encoding" | "connection" | "content-length") {
                r.headers_mut().append(k, v.clone());
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_urls() {
        assert_eq!(split_bucket_url("s3://b/do/").unwrap(), ("b".into(), "do".into()));
        assert_eq!(split_bucket_url("s3://b").unwrap(), ("b".into(), String::new()));
        assert!(split_bucket_url("http://b").is_err());
    }

    const OWN: &str = "do/acct/";

    fn record(node: &str, expires_ms: u64) -> Vec<u8> {
        format!(r#"{{"node":"{node}","address":"10.0.0.1:7000","expires_ms":{expires_ms},"instance":42,"draining":false,"later":"kept"}}"#)
            .into_bytes()
    }

    #[test]
    fn lease_records_are_recognized() {
        let seen = lease_record(OWN, "do/acct/v1/nodes/ab12.json", &record("ab12", 5)).unwrap();
        assert_eq!(
            seen,
            NodeRecord { key: "do/acct/v1/nodes/ab12.json".into(), node: "ab12".into(), instance: 42 }
        );
        // Not a lease record, another node's name, given up, or another layout.
        assert!(lease_record(OWN, "do/acct/v1/nodes/ab12.probe", &record("ab12", 5)).is_none());
        assert!(lease_record(OWN, "do/acct/v1/nodes/ab12.json", &record("cd34", 5)).is_none());
        assert!(lease_record(OWN, "do/acct/v1/nodes/ab12.json", &record("ab12", 0)).is_none());
        assert!(lease_record(OWN, "do/acct/v2/nodes/ab12.json", &record("ab12", 5)).is_none());
        assert!(lease_record(OWN, "do/other/v1/nodes/ab12.json", &record("ab12", 5)).is_none());
        assert!(lease_record(OWN, "do/acct/v1/nodes/x/ab12.json", &record("ab12", 5)).is_none());
        assert!(lease_record(OWN, "do/acct/v1/nodes/ab12.json", b"not json").is_none());
    }

    #[test]
    fn expiring_keeps_other_fields() {
        let seen = lease_record(OWN, "do/acct/v1/nodes/ab12.json", &record("ab12", 5)).unwrap();
        let out: serde_json::Value =
            serde_json::from_slice(&expired(&record("ab12", 9), &seen).unwrap()).unwrap();
        assert_eq!(out["expires_ms"], 0);
        assert_eq!(out["draining"], true);
        assert_eq!(out["instance"], 42);
        assert_eq!(out["later"], "kept");
        // Given up already, or written by another process.
        assert!(expired(&record("ab12", 0), &seen).is_none());
        let other = String::from_utf8(record("ab12", 9)).unwrap().replace("42", "43");
        assert!(expired(other.as_bytes(), &seen).is_none());
    }
}
