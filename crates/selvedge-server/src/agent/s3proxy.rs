//! A per-agent S3 proxy for Durable Object storage, so that no workerd process
//! holds the object store's credentials.
//!
//! Each account's workerd gets an access key id equal to its account id and a
//! secret derived from the cluster secret, both valid only here. The proxy
//! checks the request's SigV4 signature with that account's secret, allows
//! only the account's own prefix (`<prefix>/<account>/...`) and the operations
//! workerd's cluster mode uses, then signs the request again with the real
//! credentials and forwards it. A compromised process can therefore read and
//! write only its own account's Durable Objects, whatever the object store
//! supports in the way of scoped credentials.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use percent_encoding::{percent_decode_str, utf8_percent_encode};
use selvedge_core::AccountId;

use crate::proxy::{self, Body};
use crate::sigv4::{AWS_PATH, amz_date_secs, hmac, parse_authorization, signature};

/// The largest object the proxy carries. Durable Object storage writes are
/// far smaller.
const MAX_BODY: usize = 256 << 20;
/// How far a request's `x-amz-date` may be from now, as S3 allows.
const MAX_SKEW_SECS: i64 = 15 * 60;

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

pub struct S3Proxy {
    upstream: Upstream,
    bucket: String,
    /// The cluster prefix inside the bucket, without slashes at either end.
    prefix: String,
    secret: String,
    http: reqwest::Client,
}

/// The secret an account's workerd signs its requests with.
pub fn account_secret(cluster_secret: &str, account: &AccountId) -> String {
    hex::encode(hmac(cluster_secret.as_bytes(), format!("selvedge s3 proxy v1\0{account}").as_bytes()))
}

/// Splits `s3://bucket/prefix` into the bucket and the prefix.
pub fn split_bucket_url(url: &str) -> Result<(String, String)> {
    let rest = url.strip_prefix("s3://").context("the Durable Object bucket must be an s3:// URL")?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    anyhow::ensure!(!bucket.is_empty(), "the Durable Object bucket URL has no bucket");
    Ok((bucket.to_owned(), prefix.trim_matches('/').to_owned()))
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
    pub fn new(upstream: Upstream, bucket_url: &str, secret: String) -> Result<Arc<Self>> {
        let (bucket, prefix) = split_bucket_url(bucket_url)?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        Ok(Arc::new(Self {
            upstream: Upstream { endpoint: upstream.endpoint.trim_end_matches('/').to_owned(), ..upstream },
            bucket,
            prefix,
            secret,
            http,
        }))
    }

    /// The account's key prefix inside the bucket.
    fn account_prefix(&self, account: &AccountId) -> String {
        if self.prefix.is_empty() { format!("{account}/") } else { format!("{}/{account}/", self.prefix) }
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

    async fn handle(&self, req: Request<Incoming>) -> Response<Body> {
        match self.check(&req) {
            Ok(path) => self.forward(req, &path).await,
            Err((status, code, msg)) => {
                tracing::warn!(method = %req.method(), path = %req.uri().path(), %code, msg, "S3 proxy refused a request");
                s3_error(status, code, msg)
            }
        }
    }

    /// Authenticates and authorizes a request. Returns its canonical path.
    fn check(&self, req: &Request<Incoming>) -> Result<String, (StatusCode, &'static str, &'static str)> {
        let denied = (StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
        let h = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
        let auth = h("authorization").and_then(parse_authorization).ok_or((
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "missing or malformed SigV4 authorization",
        ))?;
        let account: AccountId = auth.access_key_id.parse().map_err(|_| denied)?;
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
            &account_secret(&self.secret, &account),
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
        let own = self.account_prefix(&account);
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
                if ok { Ok(canonical_path) } else { Err(denied) }
            }
            Some(k) if k.starts_with(&own) => {
                let allowed = [Method::GET, Method::HEAD, Method::PUT, Method::DELETE, Method::POST];
                if allowed.contains(req.method()) { Ok(canonical_path) } else { Err(denied) }
            }
            _ => Err(denied),
        }
    }

    async fn forward(&self, req: Request<Incoming>, canonical_path: &str) -> Response<Body> {
        let (parts, body) = req.into_parts();
        let body = match http_body_util::Limited::new(body, MAX_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return s3_error(StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge", "object too large"),
        };
        let query = parts.uri.query().unwrap_or("");
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
            parts.method.as_str(),
            canonical_path,
            query,
            &signed,
            payload_hash,
            &amz_date,
            &self.upstream.region,
        );
        let names: Vec<&str> = signed.iter().map(|(k, _)| k.as_str()).collect();
        let mut out = self.http.request(parts.method.clone(), url).body(body);
        for (k, v) in &signed[1..] {
            out = out.header(k.as_str(), v.as_str());
        }
        out = out.header(
            "authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{}/{}/s3/aws4_request, SignedHeaders={}, Signature={sig}",
                self.upstream.access_key_id,
                &amz_date[..8],
                self.upstream.region,
                names.join(";")
            ),
        );
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
}
