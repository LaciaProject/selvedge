//! Workers Custom Domains (`/workers/domains`): a hostname served by a script.

use axum::Router;
use axum::extract::{Path, State as AxState};
use axum::response::Response;
use axum::routing::{delete, get};
use selvedge_core::validate_script_name;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, State, account_access};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/client/v4/accounts/{account}/workers/domains", get(list).put(attach))
        .route("/client/v4/accounts/{account}/workers/domains/{id}", delete(detach))
}

/// A DNS hostname: labels of letters, digits and hyphens.
fn valid_hostname(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').count() >= 2
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[derive(Deserialize)]
struct AttachBody {
    hostname: String,
    service: String,
    #[serde(default)]
    environment: Option<String>,
}

async fn attach(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    super::JsonBody(body): super::JsonBody<AttachBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let hostname = body.hostname.to_ascii_lowercase();
    if !valid_hostname(&hostname) {
        return Err(ApiError::bad_request("invalid hostname"));
    }
    if let Some(zone) = &state.workers_dev_zone {
        if hostname == *zone || hostname.ends_with(&format!(".{zone}")) {
            return Err(ApiError::bad_request(
                "hostnames under the workers.dev zone are assigned automatically",
            ));
        }
    }
    validate_script_name(&body.service).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if state.db.script(&id, &body.service).await?.is_none() {
        return Err(ApiError::bad_request(format!("Worker `{}` does not exist", body.service)));
    }
    let domain_id = state.db.put_domain(&id, &hostname, &body.service).await?.ok_or_else(|| {
        ApiError::new(http::StatusCode::CONFLICT, 100117, "Hostname is already in use by another account")
    })?;
    Ok(ok(json!({
        "id": domain_id,
        "hostname": hostname,
        "service": body.service,
        "environment": body.environment.unwrap_or_else(|| "production".into()),
        "zone_id": "",
        "zone_name": "",
    })))
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let domains = state.db.domains(&id).await?;
    Ok(ok(json!(
        domains
            .iter()
            .map(|(id, hostname, service)| json!({
                "id": id, "hostname": hostname, "service": service, "environment": "production",
            }))
            .collect::<Vec<_>>()
    )))
}

async fn detach(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, domain)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    if !state.db.delete_domain(&id, &domain).await? {
        return Err(ApiError::not_found("Domain not found"));
    }
    Ok(ok(Value::Null))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hostnames() {
        assert!(super::valid_hostname("api.example.com"));
        assert!(!super::valid_hostname("localhost"));
        assert!(!super::valid_hostname("-a.example.com"));
        assert!(!super::valid_hostname("a..b"));
        assert!(!super::valid_hostname("A.example.com"));
    }
}
