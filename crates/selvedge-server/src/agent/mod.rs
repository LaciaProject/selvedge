//! The runner agent: receives requests from gateways and runs them on the
//! account's workerd process.
//!
//! Gateways send `x-selvedge-account`, `x-selvedge-script` and the internal token
//! in `x-selvedge-token`. Every `x-selvedge-*` header is removed before the
//! request reaches tenant code.

mod bindings;
pub mod dockershim;
pub mod limits;
mod manager;
pub mod s3proxy;
pub mod uids;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use selvedge_core::AccountId;

use crate::proxy::{self, Body, Target};

pub use manager::Manager;

pub const ACCOUNT_HEADER: &str = "x-selvedge-account";
pub const SCRIPT_HEADER: &str = "x-selvedge-script";
pub const TOKEN_HEADER: &str = "x-selvedge-token";

#[derive(Clone, Debug)]
pub struct AgentCluster {
    /// `s3://bucket/prefix`; each account gets `<prefix>/<account>`.
    pub bucket: String,
    pub bucket_options: Vec<String>,
    /// The IP workerd binds its cluster port on (the pod IP).
    pub bind_host: String,
    /// Shared by every agent; per-account cluster keys derive from it.
    pub secret: String,
}

/// Bucket options that carry the object store's endpoint or credentials.
/// workerd never gets them: it talks to the agent's S3 proxy instead.
pub const UPSTREAM_OPTIONS: &[&str] =
    &["aws_endpoint", "aws_access_key_id", "aws_secret_access_key", "aws_session_token", "aws_token"];

impl AgentCluster {
    fn option(&self, key: &str) -> Option<String> {
        self.bucket_options
            .iter()
            .find_map(|o| o.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v.to_owned()))
    }

    /// The real object store, from the bucket options or the agent's `AWS_*`
    /// environment.
    pub fn upstream(&self) -> Result<s3proxy::Upstream> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let region =
            self.option("aws_region").or_else(|| env("AWS_REGION")).unwrap_or_else(|| "us-east-1".into());
        let endpoint = self
            .option("aws_endpoint")
            .or_else(|| env("AWS_ENDPOINT_URL"))
            .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
        Ok(s3proxy::Upstream {
            endpoint,
            access_key_id: self.option("aws_access_key_id").or_else(|| env("AWS_ACCESS_KEY_ID")).ok_or_else(
                || anyhow::anyhow!("no object store access key for Durable Objects (AWS_ACCESS_KEY_ID)"),
            )?,
            secret_access_key: self
                .option("aws_secret_access_key")
                .or_else(|| env("AWS_SECRET_ACCESS_KEY"))
                .ok_or_else(|| {
                    anyhow::anyhow!("no object store secret key for Durable Objects (AWS_SECRET_ACCESS_KEY)")
                })?,
            session_token: self.option("aws_session_token").or_else(|| env("AWS_SESSION_TOKEN")),
            region,
        })
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub listen: SocketAddr,
    pub api_url: String,
    /// The data service, `host:port`.
    pub data_addr: String,
    pub internal_token: String,
    pub workerd: PathBuf,
    pub state_dir: PathBuf,
    pub run_dir: PathBuf,
    pub idle_timeout: Duration,
    pub cluster: Option<AgentCluster>,
    pub allow_local_durable_objects: bool,
    /// After a shutdown signal, how long to keep serving while reporting
    /// unhealthy, so gateways stop sending new requests before the drain.
    pub drain_delay: Duration,
    /// Bound on the local static asset cache (`<state_dir>/assets`).
    pub asset_cache_bytes: u64,
    /// Run each account's workerd as its own user, from this range.
    pub account_uids: Option<std::ops::Range<u32>>,
    /// CPU, memory and process limits per account process.
    pub limits: Option<limits::Limits>,
    /// Kill an account process that has not answered a health probe for this
    /// long (a request stuck in a loop blocks the whole process).
    pub hang_timeout: Option<Duration>,
    /// Run Durable Objects' containers (RFC 0002).
    pub containers: Option<dockershim::ContainersConfig>,
}

pub async fn run(cfg: AgentConfig, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    if uids::euid() == 0 && cfg.account_uids.is_none() {
        anyhow::bail!(
            "the agent is running as root without --account-uids: tenant code would run as root. \
             Set --account-uids (e.g. 10000-59999) or run the agent as an unprivileged user"
        );
    }
    std::fs::create_dir_all(&cfg.state_dir)?;
    std::fs::create_dir_all(&cfg.run_dir)?;
    if cfg.account_uids.is_some() {
        uids::check_executable_by_others(&cfg.workerd)?;
        // Account users pass through these to their own 0700 directories, but
        // cannot list them.
        let accounts = cfg.state_dir.join("a");
        std::fs::create_dir_all(&accounts)?;
        for dir in [&cfg.state_dir, &accounts, &cfg.run_dir] {
            uids::traversable(dir)?;
        }
    }
    let max_date = selvedge_workerd::workerd_max_date(&cfg.workerd).await?;
    tracing::info!(workerd = %cfg.workerd.display(), %max_date, "agent starting");
    let cfg = Arc::new(cfg);
    // Durable Object storage goes through the agent's S3 proxy, so workerd
    // processes hold only credentials of their own.
    let s3_proxy = match &cfg.cluster {
        Some(c) => {
            let proxy = s3proxy::S3Proxy::new(c.upstream()?, &c.bucket)?;
            let addr = proxy.clone().serve(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
            tracing::info!(%addr, "Durable Object storage proxy listening");
            Some((proxy, addr))
        }
        None => None,
    };
    let limit_mode = cfg.limits.as_ref().map(|_| limits::detect());
    let cgroup_dir = match &limit_mode {
        Some(limits::Mode::Cgroup(d)) => Some(d.clone()),
        _ => None,
    };
    let shim = match &cfg.containers {
        Some(c) => {
            let shim = dockershim::Shim::new(c.clone(), cfg.internal_token.clone());
            shim.start().await?;
            Some(shim)
        }
        None => None,
    };
    let manager = Manager::new(cfg.clone(), max_date, s3_proxy, cgroup_dir, shim)?;
    let watchdog = match (&cfg.limits, &limit_mode) {
        (Some(l), Some(limits::Mode::Watchdog)) => {
            let (manager, l) = (manager.clone(), l.clone());
            Some(tokio::spawn(async move {
                let mut w = limits::Watchdog::default();
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let pids = manager.live_pids().await;
                    for (account, action) in w.check(&pids, &l) {
                        let pid = pids.iter().find(|(a, _)| *a == account).map(|(_, p)| *p);
                        if let (limits::Action::Kill, Some(pid)) = (action, pid) {
                            // A restart can take a while (a Durable Object
                            // handoff); other accounts are still checked.
                            let manager = manager.clone();
                            tokio::spawn(async move { manager.restart_over_limit(&account, pid).await });
                        }
                    }
                }
            }))
        }
        _ => None,
    };

    let evictor = {
        let manager = manager.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                manager.evict_idle().await;
            }
        })
    };

    let pruner = {
        let dir = cfg.state_dir.join("assets");
        let max = cfg.asset_cache_bytes;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let dir = dir.clone();
                let _ = tokio::task::spawn_blocking(move || bindings::prune_asset_cache(&dir, max)).await;
            }
        })
    };
    let watcher = tokio::spawn(manager.clone().watch());
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    tracing::info!(addr = %cfg.listen, "agent listening");
    let draining = Arc::new(AtomicBool::new(false));
    let handler = {
        let cfg = cfg.clone();
        let manager = manager.clone();
        let draining = draining.clone();
        Arc::new(move |req: Request<Incoming>| {
            let cfg = cfg.clone();
            let manager = manager.clone();
            let draining = draining.clone();
            async move { handle(&cfg, &manager, &draining, req).await }
        })
    };
    let accept = |accepted: std::io::Result<(tokio::net::TcpStream, SocketAddr)>| match accepted {
        Ok((stream, _)) => {
            let _ = stream.set_nodelay(true);
            let handler = handler.clone();
            // With mTLS, only gateways get in (and this runner's own probes,
            // to /healthz only).
            tokio::spawn(async move {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::tls::accept(stream, crate::tls::AGENT_CLIENTS),
                )
                .await
                {
                    Ok(Ok(stream)) => {
                        let health_only = stream.health_only();
                        let gated = Arc::new(move |mut req: Request<Incoming>| {
                            let handler = handler.clone();
                            async move {
                                if health_only {
                                    if !crate::tls::health_path(req.uri().path()) {
                                        return proxy::full(http::StatusCode::FORBIDDEN, "forbidden");
                                    }
                                    // A probe asks about this runner, never an account.
                                    proxy::strip_internal(req.headers_mut());
                                }
                                handler(req).await
                            }
                        });
                        proxy::serve_connection(stream, gated)
                    }
                    Ok(Err(e)) => tracing::debug!(error = %e, "TLS handshake failed"),
                    Err(_) => tracing::debug!("TLS handshake timed out"),
                }
            });
        }
        Err(e) => tracing::warn!(error = %e, "accept failed"),
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => accept(accepted),
            () = &mut shutdown => break,
        }
    }
    // Fail health checks but keep serving, so gateways move away first.
    draining.store(true, Ordering::SeqCst);
    tracing::info!(delay = ?cfg.drain_delay, "agent unhealthy; waiting before draining");
    let delay = tokio::time::sleep(cfg.drain_delay);
    tokio::pin!(delay);
    loop {
        tokio::select! {
            accepted = listener.accept() => accept(accepted),
            () = &mut delay => break,
        }
    }
    tracing::info!("agent draining");
    evictor.abort();
    pruner.abort();
    if let Some(w) = watchdog {
        w.abort();
    }
    watcher.abort();
    manager.stop_all().await;
    Ok(())
}

/// Whether a request goes to a gradual deployment's canary, as on Cloudflare:
/// `Cloudflare-Workers-Version-Overrides: <script>="<version id>"` picks a
/// version; `Cloudflare-Workers-Version-Key` keeps a client on one version;
/// otherwise each request is assigned at random by the percentages.
fn pick_canary(headers: &http::HeaderMap, script: &str, share: f64, canary: &str) -> bool {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    if let Some(overrides) = h("cloudflare-workers-version-overrides") {
        for item in overrides.split(',') {
            if let Some((name, version)) = item.split_once('=')
                && name.trim() == script
            {
                return version.trim().trim_matches('"') == canary;
            }
        }
    }
    let roll = match h("cloudflare-workers-version-key") {
        Some(key) => {
            let d = selvedge_core::sha256_hex(format!("{script}\0{key}").as_bytes());
            (u64::from_str_radix(&d[..12], 16).unwrap_or_default() % 10_000) as f64 / 100.0
        }
        None => rand::random::<f64>() * 100.0,
    };
    roll < share
}

async fn handle(
    cfg: &AgentConfig,
    manager: &Arc<Manager>,
    draining: &AtomicBool,
    req: Request<Incoming>,
) -> Response<Body> {
    if req.uri().path() == "/healthz" && !req.headers().contains_key(ACCOUNT_HEADER) {
        return if draining.load(Ordering::SeqCst) {
            proxy::full(StatusCode::SERVICE_UNAVAILABLE, "draining")
        } else {
            proxy::full(StatusCode::OK, "ok")
        };
    }
    let start = std::time::Instant::now();
    let resp = run_request(cfg, manager, req).await;
    crate::metrics::request("agent", resp.status(), start.elapsed());
    resp
}

async fn run_request(
    cfg: &AgentConfig,
    manager: &Arc<Manager>,
    mut req: Request<Incoming>,
) -> Response<Body> {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let authorized = header(TOKEN_HEADER)
        .is_some_and(|t| crate::constant_time_eq(t.as_bytes(), cfg.internal_token.as_bytes()));
    if !authorized {
        return proxy::full(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let (Some(account), Some(script)) = (header(ACCOUNT_HEADER), header(SCRIPT_HEADER)) else {
        return proxy::full(StatusCode::BAD_REQUEST, "missing routing headers");
    };
    let Ok(account) = account.parse::<AccountId>() else {
        return proxy::full(StatusCode::BAD_REQUEST, "bad account");
    };
    // `request.cf` from the gateway if it sent a JSON object; otherwise an
    // empty one, so `request.cf.country` reads undefined instead of throwing.
    let cf = header(selvedge_workerd::render::CF_HEADER)
        .filter(|v| serde_json::from_str::<serde_json::Value>(v).is_ok_and(|j| j.is_object()))
        .unwrap_or_else(|| "{}".to_owned());
    proxy::strip_internal(req.headers_mut());
    if let Ok(v) = http::HeaderValue::from_str(&cf) {
        req.headers_mut().insert(selvedge_workerd::render::CF_HEADER, v);
    }

    let live = match manager.ensure(&account).await {
        Ok(manager::Lookup::Ready(live)) => live,
        Ok(manager::Lookup::NotFound) => return proxy::full(StatusCode::NOT_FOUND, "Worker not found"),
        Err(e) => {
            tracing::error!(%account, error = %format!("{e:#}"), "account process unavailable");
            return proxy::full(StatusCode::SERVICE_UNAVAILABLE, "Worker unavailable");
        }
    };
    let target = match live.splits.get(&script) {
        Some((share, canary)) if pick_canary(req.headers(), &script, *share, canary) => {
            selvedge_workerd::render::canary_socket(&script)
        }
        _ => script.clone(),
    };
    let Some(socket) = live.sockets.get(&target) else {
        return proxy::full(StatusCode::NOT_FOUND, "Worker not found");
    };
    match proxy::forward(Target::Unix(socket), req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(%account, %script, error = %e, "forwarding to workerd failed");
            proxy::full(StatusCode::BAD_GATEWAY, "Worker unavailable")
        }
    }
}
