//! The gateway: maps a request's hostname to an account and script, and sends it
//! to one of the runners chosen for that account.
//!
//! Runners are chosen by rendezvous hashing over the healthy runners, so every
//! gateway picks the same small set for an account without coordination, and a
//! runner joining or leaving moves only its share. Each request goes to a random
//! member of the account's set, which keeps the set warm; if a connection to one
//! cannot be opened, the next is tried with the same request.
//!
//! The routing table is fetched from the API and cached; while the API is down
//! the gateway keeps routing with the last table it had.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use http::{HeaderValue, Request, Response, StatusCode};
use hyper::body::Incoming;
use selvedge_store::Route;
use sha2::{Digest, Sha256};

use crate::agent::{ACCOUNT_HEADER, SCRIPT_HEADER, TOKEN_HEADER};
use crate::proxy::{self, Body, Target};

#[derive(Clone, Debug)]
pub enum Runners {
    /// Fixed `host:port` list.
    Static(Vec<String>),
    /// A DNS name resolving to every runner (a headless Service), and the port.
    Dns { host: String, port: u16 },
}

#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub listen: SocketAddr,
    /// For platform components (the API, the data service): requests naming
    /// an account and script directly. With mTLS this is the only listener
    /// that honors them, and it requires a platform certificate.
    pub internal_listen: Option<SocketAddr>,
    pub api_url: String,
    pub internal_token: String,
    pub runners: Runners,
    /// How many runners serve each account.
    pub replicas: usize,
    /// Trust `x-forwarded-for`/`x-forwarded-proto` from the load balancer.
    pub trust_forwarded: bool,
}

#[derive(Default)]
struct Shared {
    routes: RwLock<HashMap<String, Route>>,
    /// Zone route patterns, tried when no hostname matches.
    patterns: RwLock<Vec<selvedge_store::zones::PatternRoute>>,
    /// Runner addresses that passed their last health check, sorted.
    healthy: RwLock<Vec<String>>,
}

pub async fn run(cfg: GatewayConfig, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    let cfg = Arc::new(cfg);
    let shared = Arc::new(Shared::default());
    let http = crate::tls::reqwest(reqwest::Client::builder()).timeout(Duration::from_secs(5)).build()?;

    // Load routes once before serving, but do not fail if the API is down:
    // a restarted gateway should come up and serve once the API is back.
    let mut etag = None;
    if let Err(e) = refresh_routes(&http, &cfg, &shared, &mut etag).await {
        tracing::warn!(error = %format!("{e:#}"), "initial route load failed");
    }
    check_runners(&http, &cfg, &shared).await;
    let mut tasks = vec![
        tokio::spawn({
            let (http, cfg, shared) = (http.clone(), cfg.clone(), shared.clone());
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = refresh_routes(&http, &cfg, &shared, &mut etag).await {
                        tracing::warn!(error = %format!("{e:#}"), "route refresh failed; keeping the last table");
                    }
                }
            }
        }),
        tokio::spawn({
            let (http, cfg, shared) = (http.clone(), cfg.clone(), shared.clone());
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    check_runners(&http, &cfg, &shared).await;
                }
            }
        }),
    ];

    if let Some(addr) = cfg.internal_listen {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "gateway internal listener");
        let (cfg, shared) = (cfg.clone(), shared.clone());
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else { continue };
                let (cfg, shared) = (cfg.clone(), shared.clone());
                tokio::spawn(async move {
                    let stream =
                        match tokio::time::timeout(Duration::from_secs(10), crate::tls::accept(stream)).await
                        {
                            Ok(Ok(s)) => s,
                            _ => return,
                        };
                    let handler = Arc::new(move |req| {
                        let (cfg, shared) = (cfg.clone(), shared.clone());
                        async move { handle(&cfg, &shared, peer, req, true).await }
                    });
                    proxy::serve_connection(stream, handler);
                });
            }
        }));
    }
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    tracing::info!(addr = %cfg.listen, "gateway listening");
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    let (cfg, shared) = (cfg.clone(), shared.clone());
                    let handler = Arc::new(move |req| {
                        let (cfg, shared) = (cfg.clone(), shared.clone());
                        async move { handle(&cfg, &shared, peer, req, false).await }
                    });
                    proxy::serve_connection(stream, handler);
                }
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },
            () = &mut shutdown => break,
        }
    }
    for t in tasks {
        t.abort();
    }
    Ok(())
}

async fn refresh_routes(
    http: &reqwest::Client,
    cfg: &GatewayConfig,
    shared: &Shared,
    etag: &mut Option<String>,
) -> Result<()> {
    let mut req = http.get(format!("{}/internal/v1/routes", cfg.api_url)).bearer_auth(&cfg.internal_token);
    if let Some(e) = etag.as_ref() {
        req = req.header("if-none-match", e);
    }
    let resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(());
    }
    let resp = resp.error_for_status()?;
    let new_etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_owned);
    #[derive(serde::Deserialize)]
    struct Table {
        routes: Vec<Route>,
        #[serde(default)]
        patterns: Vec<selvedge_store::zones::PatternRoute>,
    }
    let table: Table = resp.json().await?;
    let map = table.routes.into_iter().map(|r| (r.hostname.clone(), r)).collect();
    *shared.routes.write().expect("poisoned") = map;
    *shared.patterns.write().expect("poisoned") = table.patterns;
    *etag = new_etag;
    Ok(())
}

async fn runner_addresses(cfg: &GatewayConfig) -> Vec<String> {
    match &cfg.runners {
        Runners::Static(list) => list.clone(),
        Runners::Dns { host, port } => match tokio::net::lookup_host((host.as_str(), *port)).await {
            Ok(addrs) => addrs.map(|a| a.to_string()).collect(),
            Err(e) => {
                tracing::warn!(%host, error = %e, "resolving runners failed");
                vec![]
            }
        },
    }
}

async fn check_runners(http: &reqwest::Client, cfg: &GatewayConfig, shared: &Shared) {
    let addrs: HashSet<String> = runner_addresses(cfg).await.into_iter().collect();
    let checks = addrs.into_iter().map(|addr| {
        let http = http.clone();
        async move {
            let ok = http
                .get(format!("{}://{addr}/healthz", crate::tls::scheme()))
                .timeout(Duration::from_secs(1))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            ok.then_some(addr)
        }
    });
    let mut healthy: Vec<String> = futures::future::join_all(checks).await.into_iter().flatten().collect();
    healthy.sort();
    let mut current = shared.healthy.write().expect("poisoned");
    if *current != healthy {
        tracing::info!(runners = ?healthy, "healthy runners changed");
        *current = healthy;
    }
}

/// The runners for an account, best first (rendezvous hashing).
pub fn rank(account: &str, runners: &[String]) -> Vec<String> {
    let mut scored: Vec<_> = runners
        .iter()
        .map(|r| {
            let mut h = Sha256::new();
            h.update(account.as_bytes());
            h.update([0]);
            h.update(r.as_bytes());
            (h.finalize(), r.clone())
        })
        .collect();
    scored.sort_by_key(|s| std::cmp::Reverse(s.0));
    scored.into_iter().map(|(_, r)| r).collect()
}

fn hostname(req: &Request<Incoming>) -> Option<String> {
    let host =
        req.headers().get(http::header::HOST).and_then(|v| v.to_str().ok()).or_else(|| req.uri().host())?;
    // Strip a port, keeping IPv6 literals intact.
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => host.split(':').next().unwrap_or(host),
    };
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

async fn handle(
    cfg: &GatewayConfig,
    shared: &Shared,
    peer: SocketAddr,
    mut req: Request<Incoming>,
    internal_listener: bool,
) -> Response<Body> {
    if req.uri().path() == "/__selvedge/healthz" {
        return proxy::full(StatusCode::OK, "ok");
    }
    // Platform components (the API) may name the account and script directly.
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    // With mTLS, direct routing needs the internal listener (and so a
    // platform certificate) as well as the token.
    let internal = (internal_listener || crate::tls::get().is_none())
        && header(TOKEN_HEADER)
            .is_some_and(|t| crate::constant_time_eq(t.as_bytes(), cfg.internal_token.as_bytes()));
    let direct = match (internal, header(ACCOUNT_HEADER), header(SCRIPT_HEADER)) {
        (true, Some(account), Some(service)) => {
            account.parse().ok().map(|account_id| Route { hostname: String::new(), account_id, service })
        }
        _ => None,
    };
    let route = match direct {
        Some(r) => r,
        None => {
            let Some(host) = hostname(&req) else {
                return proxy::full(StatusCode::BAD_REQUEST, "missing Host header");
            };
            let exact = shared.routes.read().expect("poisoned").get(&host).cloned();
            let route = exact.or_else(|| {
                let patterns = shared.patterns.read().expect("poisoned");
                let i = selvedge_core::routes::best(
                    patterns.iter().map(|p| p.pattern.as_str()),
                    &host,
                    req.uri().path(),
                )?;
                let p = &patterns[i];
                // A route without a script: matching requests skip Workers.
                (!p.service.is_empty()).then(|| Route {
                    hostname: host.clone(),
                    account_id: p.account_id.clone(),
                    service: p.service.clone(),
                })
            });
            let Some(route) = route else {
                return proxy::full(
                    StatusCode::NOT_FOUND,
                    "There is nothing here yet. (Selvedge: no Worker is routed for this hostname)",
                );
            };
            route
        }
    };

    let version = req.version();
    let headers = req.headers_mut();
    proxy::strip_internal(headers);
    let client_ip = if cfg.trust_forwarded {
        headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim().to_owned())
            .unwrap_or_else(|| peer.ip().to_string())
    } else {
        headers.remove("x-forwarded-proto");
        peer.ip().to_string()
    };
    if let Ok(v) = HeaderValue::from_str(&client_ip) {
        headers.insert("cf-connecting-ip", v.clone());
        headers.insert("x-real-ip", v);
    }
    if !headers.contains_key("x-forwarded-proto") {
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    }
    let (Ok(account), Ok(script), Ok(token)) = (
        HeaderValue::from_str(route.account_id.as_str()),
        HeaderValue::from_str(&route.service),
        HeaderValue::from_str(&cfg.internal_token),
    ) else {
        return proxy::full(StatusCode::INTERNAL_SERVER_ERROR, "bad route");
    };
    headers.insert(ACCOUNT_HEADER, account);
    headers.insert(SCRIPT_HEADER, script);
    headers.insert(TOKEN_HEADER, token);
    // `request.cf`: only what the gateway knows. No geo or network data.
    let cf = serde_json::json!({ "httpProtocol": format!("{:?}", version) }).to_string();
    if let Ok(v) = HeaderValue::from_str(&cf) {
        headers.insert(selvedge_workerd::render::CF_HEADER, v);
    }

    let healthy = shared.healthy.read().expect("poisoned").clone();
    let mut ranked = rank(route.account_id.as_str(), &healthy);
    if ranked.is_empty() {
        return proxy::full(StatusCode::SERVICE_UNAVAILABLE, "no runners available");
    }
    // Spread over the account's set; the rest of the ranking is the fallback.
    let set = cfg.replicas.clamp(1, ranked.len());
    let first = rand::random_range(0..set);
    ranked[..set].rotate_left(first);

    for addr in &ranked {
        match proxy::connect(Target::Tcp(addr)).await {
            Ok(sender) => {
                return match proxy::send(sender, req).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(runner = %addr, error = %e, "runner request failed");
                        proxy::full(StatusCode::BAD_GATEWAY, "Worker unavailable")
                    }
                };
            }
            Err(e) => {
                tracing::warn!(runner = %addr, error = %e, "runner unreachable; trying the next");
                shared.healthy.write().expect("poisoned").retain(|r| r != addr);
            }
        }
    }
    proxy::full(StatusCode::SERVICE_UNAVAILABLE, "no runners reachable")
}

#[cfg(test)]
mod tests {
    use super::rank;

    #[test]
    fn rendezvous_is_stable_and_minimal() {
        let runners: Vec<String> = (0..5).map(|i| format!("10.0.0.{i}:9000")).collect();
        let a = rank("acct", &runners);
        assert_eq!(a, rank("acct", &runners));
        assert_eq!(a.len(), 5);
        // Removing a runner that is not in the top two keeps the top two.
        let removed = a[4].clone();
        let fewer: Vec<_> = runners.iter().filter(|r| **r != removed).cloned().collect();
        assert_eq!(rank("acct", &fewer)[..2], a[..2]);
    }
}
