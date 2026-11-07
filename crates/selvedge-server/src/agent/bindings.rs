//! The per-account binding socket. workerd's external binding services (KV)
//! connect here; the agent stamps the account identity that the socket stands
//! for and forwards to the data service. Tenant code cannot open Unix sockets,
//! so only workerd's own binding calls arrive here.
//!
//! Static asset reads are immutable (keyed by content hash), so they are cached
//! on local disk: a runner keeps serving assets it has seen while the data
//! service is unreachable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use http::{HeaderValue, StatusCode};
use selvedge_core::AccountId;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};

use super::AgentConfig;
use crate::data::ACCOUNT_HEADER;
use crate::proxy::{self, Body};

/// Scripts with a live tail session: `(account, script)`.
#[derive(Clone, Default)]
pub struct Tailed(pub Arc<std::sync::RwLock<std::collections::HashSet<(AccountId, String)>>>);

/// Largest batch of trace events accepted from the tail worker.
const MAX_TAIL_BYTES: usize = 4 << 20;

/// Largest asset response accepted from the data service.
const MAX_ASSET_BYTES: usize = (selvedge_core::assets::MAX_ASSET_SIZE as usize) + 1024;

pub fn serve(
    cfg: Arc<AgentConfig>,
    account: AccountId,
    path: &Path,
    tailed: Tailed,
    http: reqwest::Client,
    uid: Option<u32>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    if let Some(uid) = uid {
        // Only the account's own process may connect.
        super::uids::own(path, uid, 0o600)?;
    }
    let account_header = HeaderValue::from_str(account.as_str())?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", cfg.internal_token))?;
    let cache_dir = cfg.state_dir.join("assets").join(account.as_str());
    let handler = Arc::new(move |mut req: http::Request<hyper::body::Incoming>| {
        let cfg = cfg.clone();
        let account_header = account_header.clone();
        let auth = auth.clone();
        let cache_dir = cache_dir.clone();
        let tailed = tailed.clone();
        let http = http.clone();
        let account = account.clone();
        async move {
            if req.headers().contains_key(selvedge_workerd::render::TAIL_HEADER) {
                return forward_trace(&cfg, &http, &tailed, &account, req).await;
            }
            let asset = req.headers().contains_key(selvedge_workerd::render::ASSETS_HEADER)
                && req.method() == http::Method::GET;
            let cached = if asset { cache_path(&cache_dir, req.uri().path()) } else { None };
            if let Some(path) = &cached
                && let Some(resp) = read_cached(path).await
            {
                return resp;
            }
            let headers = req.headers_mut();
            proxy::strip_internal_except(
                headers,
                &[
                    selvedge_workerd::render::KV_NAMESPACE_HEADER,
                    selvedge_workerd::render::R2_BUCKET_HEADER,
                    selvedge_workerd::render::QUEUE_HEADER,
                    selvedge_workerd::render::ASSETS_HEADER,
                ],
            );
            headers.insert(ACCOUNT_HEADER, account_header);
            headers.insert(http::header::AUTHORIZATION, auth);
            let sent = match proxy::connect_retrying(&cfg.data_addr, crate::tls::Role::Data, 4).await {
                Ok(sender) => proxy::send(sender, req).await,
                Err(e) => Err(e),
            };
            match sent {
                Ok(r) if cached.is_some() && r.status() == StatusCode::OK => {
                    store_cached(cached.as_deref().expect("checked"), r).await
                }
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "binding request to data service failed");
                    proxy::full(StatusCode::SERVICE_UNAVAILABLE, "binding backend unavailable")
                }
            }
        }
    });
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => proxy::serve_connection(stream, handler.clone()),
                Err(e) => {
                    tracing::error!(error = %e, "binding socket accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(())
}

/// `<dir>/<hash>` for an asset read of `/<hash>`.
fn cache_path(dir: &Path, path: &str) -> Option<PathBuf> {
    let hash = path.trim_start_matches('/');
    selvedge_core::assets::valid_hash(hash).then(|| dir.join(hash))
}

fn meta_path(path: &Path) -> PathBuf {
    path.with_extension("meta")
}

fn asset_response(bytes: Bytes, meta: Option<&str>) -> http::Response<Body> {
    let mut r = http::Response::new(Full::new(bytes).map_err(|e| match e {}).boxed());
    if let Some(m) = meta.and_then(|m| HeaderValue::from_str(m).ok()) {
        r.headers_mut().insert("cf-kv-metadata", m);
    }
    r
}

async fn read_cached(path: &Path) -> Option<http::Response<Body>> {
    // The metadata file is written last, so its presence means the body is whole.
    let meta = tokio::fs::read_to_string(meta_path(path)).await.ok()?;
    let bytes = tokio::fs::read(path).await.ok()?;
    Some(asset_response(Bytes::from(bytes), Some(&meta)))
}

/// Buffers an asset from the data service, writes it to the cache and answers it.
async fn store_cached(path: &Path, resp: http::Response<Body>) -> http::Response<Body> {
    let (parts, body) = resp.into_parts();
    let meta = parts.headers.get("cf-kv-metadata").and_then(|v| v.to_str().ok()).unwrap_or("{}").to_owned();
    let bytes = match http_body_util::Limited::new(body, MAX_ASSET_BYTES).collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            tracing::error!(error = %e, "reading an asset from the data service failed");
            return proxy::full(StatusCode::BAD_GATEWAY, "asset read failed");
        }
    };
    let write = {
        let path = path.to_owned();
        let bytes = bytes.clone();
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
            let tmp = path.with_extension("metatmp");
            std::fs::write(&tmp, meta)?;
            std::fs::rename(&tmp, meta_path(&path))
        })
    };
    if let Ok(Err(e)) = write.await {
        tracing::warn!(error = %e, "caching an asset failed");
    }
    asset_response(bytes, Some(&meta))
}

/// Deletes the oldest cached assets until the cache is under 80% of `max`.
pub fn prune_asset_cache(dir: &Path, max: u64) {
    let mut files = Vec::new();
    let mut total = 0u64;
    let Ok(accounts) = std::fs::read_dir(dir) else { return };
    for account in accounts.flatten() {
        let Ok(entries) = std::fs::read_dir(account.path()) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_some() {
                continue;
            }
            if let Ok(m) = e.metadata() {
                total += m.len();
                files.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), path));
            }
        }
    }
    if total <= max {
        return;
    }
    files.sort();
    let target = max / 5 * 4;
    for (_, len, path) in files {
        if total <= target {
            break;
        }
        // The metadata file goes first, so a reader never sees a partial entry.
        let _ = std::fs::remove_file(meta_path(&path));
        let _ = std::fs::remove_file(&path);
        total = total.saturating_sub(len);
    }
}

/// Trace events from the tail worker: forwarded to the API if someone is
/// tailing the script; otherwise 204, which quiets the tail worker for a while.
async fn forward_trace(
    cfg: &AgentConfig,
    http: &reqwest::Client,
    tailed: &Tailed,
    account: &AccountId,
    req: http::Request<hyper::body::Incoming>,
) -> http::Response<Body> {
    let body = match http_body_util::Limited::new(req.into_body(), MAX_TAIL_BYTES).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return proxy::full(StatusCode::PAYLOAD_TOO_LARGE, "too many events"),
    };
    #[derive(serde::Deserialize)]
    struct Batch {
        script: String,
    }
    let Ok(batch) = serde_json::from_slice::<Batch>(&body) else {
        return proxy::full(StatusCode::BAD_REQUEST, "invalid events");
    };
    let active = tailed.0.read().expect("poisoned").contains(&(account.clone(), batch.script));
    if !active {
        return proxy::full(StatusCode::NO_CONTENT, "");
    }
    let url = format!("{}/internal/v1/accounts/{account}/tail-events", cfg.api_url);
    let req = http
        .post(url)
        .bearer_auth(&cfg.internal_token)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(body);
    // Off the request path: the tail worker does not wait for the API.
    tokio::spawn(async move {
        if let Err(e) = req.send().await.and_then(|r| r.error_for_status()) {
            tracing::debug!(error = %e, "forwarding trace events failed");
        }
    });
    proxy::full(StatusCode::ACCEPTED, "")
}
