//! HTTP/1.1 reverse proxying over TCP or Unix sockets, with upgrade (WebSocket)
//! passthrough. Used by the gateway (to runners) and the agent (to workerd and to
//! the data service).

use std::convert::Infallible;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;

pub type Body = BoxBody<Bytes, hyper::Error>;

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Headers between hops that must not be forwarded, except `upgrade`/`connection`
/// on upgrade requests.
const HOP_BY_HOP: &[&str] = &[
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

pub fn full(status: StatusCode, text: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(Full::new(text.into()).map_err(|e: Infallible| match e {}).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(http::header::UPGRADE)
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let upgrade = is_upgrade(headers);
    for h in HOP_BY_HOP {
        headers.remove(*h);
    }
    if !upgrade {
        headers.remove(http::header::CONNECTION);
        headers.remove(http::header::UPGRADE);
    }
}

/// Removes every `x-selvedge-*` header, so a client cannot spoof internal ones.
pub fn strip_internal(headers: &mut HeaderMap) {
    strip_internal_except(headers, &[]);
}

/// Like [`strip_internal`], keeping the named headers.
pub fn strip_internal_except(headers: &mut HeaderMap, keep: &[&str]) {
    let names: Vec<_> = headers
        .keys()
        .filter(|k| k.as_str().starts_with("x-selvedge-") && !keep.contains(&k.as_str()))
        .cloned()
        .collect();
    for n in names {
        headers.remove(n);
    }
}

pub enum Target<'a> {
    /// `host:port`, resolved on each connect.
    Tcp(&'a str),
    Unix(&'a Path),
}

pub type Sender = hyper::client::conn::http1::SendRequest<Incoming>;

/// Opens a connection. Failing here means nothing was sent, so the caller may
/// try another target with the same request.
pub async fn connect(target: Target<'_>) -> Result<Sender, String> {
    match target {
        Target::Tcp(addr) => {
            // A vanished pod can drop SYNs silently; fail over instead of hanging.
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr))
                .await
                .map_err(|_| format!("connect {addr}: timed out"))?
                .map_err(|e| format!("connect {addr}: {e}"))?;
            let _ = stream.set_nodelay(true);
            let (s, c) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .map_err(|e| e.to_string())?;
            spawn_conn(c);
            Ok(s)
        }
        Target::Unix(path) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| format!("connect {}: {e}", path.display()))?;
            let (s, c) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .map_err(|e| e.to_string())?;
            spawn_conn(c);
            Ok(s)
        }
    }
}

/// Sends `req` on `sender` and returns the response. If both sides agree to
/// upgrade, the two upgraded connections are joined.
pub async fn send(mut sender: Sender, mut req: Request<Incoming>) -> Result<Response<Body>, String> {
    strip_hop_by_hop(req.headers_mut());
    let client_upgrade = is_upgrade(req.headers()).then(|| hyper::upgrade::on(&mut req));
    let mut resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = client_upgrade {
            let upstream_upgrade = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let (Ok(a), Ok(b)) = (client_upgrade.await, upstream_upgrade.await) {
                    let mut a = TokioIo::new(a);
                    let mut b = TokioIo::new(b);
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
            });
        }
    }
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    Ok(Response::from_parts(parts, body.boxed()))
}

/// Connects to a `host:port` Service, retrying failed connects. Each new
/// connection may reach a different backend, so this rides out a dead pod
/// that has not yet left the Service's endpoints.
pub async fn connect_retrying(addr: &str, attempts: usize) -> Result<Sender, String> {
    let mut last = String::new();
    for attempt in 0..attempts.max(1) {
        match connect(Target::Tcp(addr)).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                tracing::debug!(%addr, attempt, error = %e, "connect failed; retrying");
                last = e;
            }
        }
    }
    Err(last)
}

pub async fn forward(target: Target<'_>, req: Request<Incoming>) -> Result<Response<Body>, String> {
    send(connect(target).await?, req).await
}

fn spawn_conn<T>(conn: hyper::client::conn::http1::Connection<TokioIo<T>, Incoming>)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!(error = %e, "upstream connection ended");
        }
    });
}

/// Serves HTTP/1.1 with upgrades on an accepted stream.
pub fn serve_connection<S, H, F>(stream: S, handler: Arc<H>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    H: Fn(Request<Incoming>) -> F + Send + Sync + 'static,
    F: Future<Output = Response<Body>> + Send + 'static,
{
    tokio::spawn(async move {
        let service = hyper::service::service_fn(move |req| {
            let handler = handler.clone();
            async move { Ok::<_, Infallible>(handler(req).await) }
        });
        if let Err(e) = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await
        {
            tracing::debug!(error = %e, "client connection ended");
        }
    });
}
