//! The per-account binding socket. workerd's external binding services (KV)
//! connect here; the agent stamps the account identity that the socket stands
//! for and forwards to the data service. Tenant code cannot open Unix sockets,
//! so only workerd's own binding calls arrive here.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use http::{HeaderValue, StatusCode};
use selvedge_core::AccountId;

use super::AgentConfig;
use crate::data::ACCOUNT_HEADER;
use crate::proxy;

pub fn serve(cfg: Arc<AgentConfig>, account: AccountId, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    let account_header = HeaderValue::from_str(account.as_str())?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", cfg.internal_token))?;
    let handler = Arc::new(move |mut req: http::Request<hyper::body::Incoming>| {
        let cfg = cfg.clone();
        let account_header = account_header.clone();
        let auth = auth.clone();
        async move {
            let headers = req.headers_mut();
            proxy::strip_internal_except(
                headers,
                &[selvedge_workerd::render::KV_NAMESPACE_HEADER, selvedge_workerd::render::R2_BUCKET_HEADER],
            );
            headers.insert(ACCOUNT_HEADER, account_header);
            headers.insert(http::header::AUTHORIZATION, auth);
            let sent = match proxy::connect_retrying(&cfg.data_addr, 4).await {
                Ok(sender) => proxy::send(sender, req).await,
                Err(e) => Err(e),
            };
            match sent {
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
