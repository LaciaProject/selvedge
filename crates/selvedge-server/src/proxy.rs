//! HTTP/1.1 reverse proxying over TCP or Unix sockets, with upgrade (WebSocket)
//! passthrough. Used by the gateway (to runners) and the agent (to workerd and to
//! the data service). Connections between components are kept alive and reused
//! ([`Pool`]), so a request does not pay for a TCP and mTLS handshake.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

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
    /// `host:port`, resolved on each connect, and the role its server must
    /// prove with mTLS.
    Tcp(&'a str, crate::tls::Role),
    Unix(&'a Path),
}

pub type Sender = hyper::client::conn::http1::SendRequest<Incoming>;

/// Opens a connection. Failing here means nothing was sent, so the caller may
/// try another target with the same request.
pub async fn connect(target: Target<'_>) -> Result<Sender, String> {
    match target {
        Target::Tcp(addr, server) => {
            crate::metrics::CONNECTIONS_OPENED.inc(&[server.label()]);
            // A vanished pod can drop SYNs silently; fail over instead of hanging.
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr))
                .await
                .map_err(|_| format!("connect {addr}: timed out"))?
                .map_err(|e| format!("connect {addr}: {e}"))?;
            let _ = stream.set_nodelay(true);
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, crate::tls::connect(stream, server))
                .await
                .map_err(|_| format!("TLS to {addr}: timed out"))?
                .map_err(|e| format!("TLS to {addr}: {e}"))?;
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

pub async fn forward(target: Target<'_>, req: Request<Incoming>) -> Result<Response<Body>, String> {
    send(connect(target).await?, req).await
}

/// Why a pooled request failed.
pub enum Failed {
    /// Nothing was sent (no connection could be opened): the request comes
    /// back, to try elsewhere.
    NotSent(Box<Request<Incoming>>, String),
    /// The request may have reached the server, so it must not be replayed.
    Sent(String),
}

/// Idle connections kept per target.
const MAX_IDLE: usize = 64;
/// A connection idle this long is closed rather than reused.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

struct Idle {
    sender: Sender,
    since: Instant,
}

/// Keep-alive HTTP/1.1 connections to other components, by address and the
/// role their server proves. A connection carries one request at a time; it
/// goes back to the pool when its response starts and is reused once that
/// response has been read ([`Sender::is_ready`]).
#[derive(Default)]
pub struct Pool {
    idle: Mutex<HashMap<(String, crate::tls::Role), Vec<Idle>>>,
}

/// The process's pool.
pub static POOL: LazyLock<Pool> = LazyLock::new(Pool::default);

impl Pool {
    fn checkout(&self, key: &(String, crate::tls::Role)) -> Option<Sender> {
        let mut idle = self.idle.lock().expect("poisoned");
        let list = idle.get_mut(key)?;
        list.retain(|i| !i.sender.is_closed() && i.since.elapsed() < IDLE_TIMEOUT);
        let ready = list.iter().position(|i| i.sender.is_ready())?;
        Some(list.swap_remove(ready).sender)
    }

    fn give_back(&self, key: (String, crate::tls::Role), sender: Sender) {
        let mut idle = self.idle.lock().expect("poisoned");
        let list = idle.entry(key).or_default();
        // A connection not kept closes once its response has ended.
        if list.len() < MAX_IDLE {
            list.push(Idle { sender, since: Instant::now() });
        }
    }

    /// Sends `req` to `addr` on a kept-alive connection, or a new one. A kept
    /// connection that turns out closed before the request was written is
    /// replaced by a new one. Upgrades (WebSockets) always get a connection of
    /// their own.
    pub async fn request(
        &self,
        addr: &str,
        server: crate::tls::Role,
        mut req: Request<Incoming>,
    ) -> Result<Response<Body>, Failed> {
        if is_upgrade(req.headers()) {
            let sender = match connect(Target::Tcp(addr, server)).await {
                Ok(s) => s,
                Err(e) => return Err(Failed::NotSent(Box::new(req), e)),
            };
            return send(sender, req).await.map_err(Failed::Sent);
        }
        strip_hop_by_hop(req.headers_mut());
        let key = (addr.to_owned(), server);
        if let Some(mut sender) = self.checkout(&key) {
            match sender.try_send_request(req).await {
                Ok(resp) => {
                    self.give_back(key, sender);
                    return Ok(finish(resp));
                }
                Err(mut e) => match e.take_message() {
                    Some(r) => req = r,
                    None => return Err(Failed::Sent(e.into_error().to_string())),
                },
            }
        }
        let mut sender = match connect(Target::Tcp(addr, server)).await {
            Ok(s) => s,
            Err(e) => return Err(Failed::NotSent(Box::new(req), e)),
        };
        let resp = sender.send_request(req).await.map_err(|e| Failed::Sent(e.to_string()))?;
        self.give_back(key, sender);
        Ok(finish(resp))
    }

    /// Like [`request`](Self::request), trying up to `attempts` connections
    /// when none can be opened. Each may reach another backend of a Service.
    pub async fn request_retrying(
        &self,
        addr: &str,
        server: crate::tls::Role,
        mut req: Request<Incoming>,
        attempts: usize,
    ) -> Result<Response<Body>, String> {
        for attempt in 1.. {
            match self.request(addr, server, req).await {
                Ok(r) => return Ok(r),
                Err(Failed::Sent(e)) => return Err(e),
                Err(Failed::NotSent(r, e)) if attempt < attempts => {
                    tracing::debug!(%addr, attempt, error = %e, "connect failed; retrying");
                    req = *r;
                }
                Err(Failed::NotSent(_, e)) => return Err(e),
            }
        }
        unreachable!("the loop returns")
    }
}

fn finish(resp: Response<Incoming>) -> Response<Body> {
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    Response::from_parts(parts, body.boxed())
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

/// Lets a server finish its connections when it stops. Each connection served
/// with [`serve_connection_draining`] closes once the request it is on has
/// been answered (at once if idle), and [`Drain::finish`] waits for all of
/// them. Upgraded connections (WebSockets) are not waited for.
pub struct Drain {
    stop: tokio::sync::watch::Sender<bool>,
    open: tokio::sync::mpsc::Sender<()>,
    closed: tokio::sync::mpsc::Receiver<()>,
}

/// One connection's part in a [`Drain`]: it holds the drain open until the
/// connection ends.
pub struct DrainGuard {
    stop: tokio::sync::watch::Receiver<bool>,
    _open: tokio::sync::mpsc::Sender<()>,
}

impl Default for Drain {
    fn default() -> Self {
        let (stop, _) = tokio::sync::watch::channel(false);
        let (open, closed) = tokio::sync::mpsc::channel(1);
        Self { stop, open, closed }
    }
}

impl Drain {
    pub fn guard(&self) -> DrainGuard {
        DrainGuard { stop: self.stop.subscribe(), _open: self.open.clone() }
    }

    /// Ask every connection to close after its current request, and wait up
    /// to `limit` for them. Answers whether they all closed in time.
    pub async fn finish(self, limit: Duration) -> bool {
        let Self { stop, open, mut closed } = self;
        let _ = stop.send(true);
        drop(open);
        // `recv` gives `None` once every connection's sender is gone.
        tokio::time::timeout(limit, closed.recv()).await.is_ok()
    }
}

/// [`serve_connection`], closing the connection gracefully when its drain
/// finishes.
pub fn serve_connection_draining<S, H, F>(stream: S, handler: Arc<H>, guard: DrainGuard)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    H: Fn(Request<Incoming>) -> F + Send + Sync + 'static,
    F: Future<Output = Response<Body>> + Send + 'static,
{
    tokio::spawn(async move {
        let DrainGuard { mut stop, _open } = guard;
        let service = hyper::service::service_fn(move |req| {
            let handler = handler.clone();
            async move { Ok::<_, Infallible>(handler(req).await) }
        });
        let conn = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades();
        tokio::pin!(conn);
        let mut stopping = false;
        loop {
            tokio::select! {
                res = conn.as_mut() => {
                    if let Err(e) = res {
                        tracing::debug!(error = %e, "client connection ended");
                    }
                    break;
                }
                // A dropped drain also stops the connection gracefully.
                _ = stop.wait_for(|s| *s), if !stopping => {
                    stopping = true;
                    conn.as_mut().graceful_shutdown();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A backend that answers every request with "hi" and counts connections.
    /// With `close_after`, it closes each connection that long after answering,
    /// as a server whose idle timeout ran out would.
    async fn backend(close_after: Option<Duration>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        let mut got = Vec::new();
                        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => got.extend_from_slice(&buf[..n]),
                            }
                        }
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi").await;
                        if let Some(d) = close_after {
                            tokio::time::sleep(d).await;
                            return;
                        }
                    }
                });
            }
        });
        (addr, accepted)
    }

    /// A front server that sends each request on through `pool`, as the
    /// gateway does, and a client for it.
    async fn front(pool: Arc<Pool>, backend: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let (pool, backend) = (pool.clone(), backend.clone());
                serve_connection(
                    s,
                    Arc::new(move |req: Request<Incoming>| {
                        let (pool, backend) = (pool.clone(), backend.clone());
                        async move {
                            match pool.request(&backend, crate::tls::Role::Runner, req).await {
                                Ok(r) => r,
                                Err(Failed::NotSent(_, e)) => full(StatusCode::SERVICE_UNAVAILABLE, e),
                                Err(Failed::Sent(e)) => full(StatusCode::BAD_GATEWAY, e),
                            }
                        }
                    }),
                );
            }
        });
        addr
    }

    async fn get(client: &reqwest::Client, front: &str) -> (u16, String) {
        let r = client.get(format!("http://{front}/")).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    /// A drain lets the request in progress finish, closes idle keep-alive
    /// connections, and reports when they are all gone.
    #[tokio::test]
    async fn drain_finishes_requests_and_closes_idle_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let drain = Drain::default();
        let handler = Arc::new(|req: Request<Incoming>| async move {
            if req.uri().path() == "/slow" {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            full(StatusCode::OK, "done")
        });
        let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (s, _) = listener.accept().await.unwrap();
        serve_connection_draining(s, handler.clone(), drain.guard());
        let mut busy = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (s, _) = listener.accept().await.unwrap();
        serve_connection_draining(s, handler, drain.guard());

        // A keep-alive request on the idle connection, answered before the drain.
        idle.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n").await.unwrap();
        let mut buf = vec![0u8; 1024];
        let n = idle.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).ends_with("done"));
        // The slow one is in progress when the drain starts.
        busy.write_all(b"GET /slow HTTP/1.1\r\nhost: x\r\n\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let finished = tokio::spawn(drain.finish(Duration::from_secs(5)));

        let mut answer = String::new();
        busy.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200") && answer.ends_with("done"), "{answer}");
        assert_eq!(idle.read(&mut buf).await.unwrap(), 0, "the idle connection is closed");
        assert!(finished.await.unwrap(), "the drain saw every connection close");
    }

    #[tokio::test]
    async fn reuses_a_connection() {
        let (b, accepted) = backend(None).await;
        let f = front(Arc::new(Pool::default()), b).await;
        let client = reqwest::Client::new();
        for _ in 0..5 {
            assert_eq!(get(&client, &f).await, (200, "hi".into()));
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn replaces_a_connection_the_server_closed() {
        let (b, accepted) = backend(Some(Duration::from_millis(20))).await;
        let f = front(Arc::new(Pool::default()), b).await;
        let client = reqwest::Client::new();
        for _ in 0..3 {
            assert_eq!(get(&client, &f).await, (200, "hi".into()));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn hands_back_a_request_it_could_not_send() {
        // Nothing listens on a port just freed.
        let addr =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().to_string();
        let f = front(Arc::new(Pool::default()), addr).await;
        assert_eq!(get(&reqwest::Client::new(), &f).await.0, 503);
    }
}
