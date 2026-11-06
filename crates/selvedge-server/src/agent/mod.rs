//! The runner agent: receives requests from gateways and runs them on the
//! account's workerd process.
//!
//! Gateways send `x-selvedge-account`, `x-selvedge-script` and the internal token
//! in `x-selvedge-token`. Every `x-selvedge-*` header is removed before the
//! request reaches tenant code.

mod bindings;
mod manager;

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
}

pub async fn run(cfg: AgentConfig, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    std::fs::create_dir_all(&cfg.run_dir)?;
    let max_date = selvedge_workerd::workerd_max_date(&cfg.workerd).await?;
    tracing::info!(workerd = %cfg.workerd.display(), %max_date, "agent starting");
    let cfg = Arc::new(cfg);
    let manager = Manager::new(cfg.clone(), max_date);

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
            proxy::serve_connection(stream, handler.clone());
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
    watcher.abort();
    manager.stop_all().await;
    Ok(())
}

async fn handle(
    cfg: &AgentConfig,
    manager: &Arc<Manager>,
    draining: &AtomicBool,
    mut req: Request<Incoming>,
) -> Response<Body> {
    if req.uri().path() == "/healthz" && !req.headers().contains_key(ACCOUNT_HEADER) {
        return if draining.load(Ordering::SeqCst) {
            proxy::full(StatusCode::SERVICE_UNAVAILABLE, "draining")
        } else {
            proxy::full(StatusCode::OK, "ok")
        };
    }
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
    let Some(socket) = live.sockets.get(&script) else {
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
