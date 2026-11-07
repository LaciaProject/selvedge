//! The control-plane API: Cloudflare's `/client/v4` subset and Selvedge's
//! internal endpoints for agents and gateways.

mod accounts;
mod assets;
mod d1;
mod domains;
mod internal;
mod kv;
mod notifications;
mod queues;
mod r2;

pub use r2::valid_bucket_name;
mod scripts;
mod tails;
mod versions;
mod workflows;
mod zones;

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use selvedge_core::AccountId;
use selvedge_store::{Blobs, Db, Principal, SecretBox};

use crate::error::{ApiError, ApiResult};

pub struct ApiState {
    pub db: Db,
    pub blobs: Blobs,
    pub secrets: SecretBox,
    /// Shared secret for agents and gateways.
    pub internal_token: String,
    /// Zone for `<script>.<subdomain>.<zone>` hostnames, like workers.dev.
    pub workers_dev_zone: Option<String>,
    /// Zone for public bucket URLs, `pub-<id>.<zone>`, like r2.dev.
    pub r2_dev_zone: Option<String>,
    /// The gateway, for requests the API sends into account processes (D1).
    pub gateway_url: Option<String>,
    pub http: reqwest::Client,
    pub tails: std::sync::Arc<crate::tail::Hub>,
}

pub type State = Arc<ApiState>;

/// Maximum upload size, matching Cloudflare's paid-plan script limit.
pub const MAX_UPLOAD: usize = 64 << 20;

pub fn router(state: State) -> Router {
    Router::new()
        .merge(accounts::routes())
        .merge(scripts::routes())
        .merge(assets::routes())
        .merge(tails::routes())
        .merge(workflows::routes())
        .merge(zones::routes())
        .merge(kv::routes())
        .merge(domains::routes())
        .merge(versions::routes())
        .merge(r2::routes())
        .merge(notifications::routes())
        .merge(d1::routes())
        .merge(queues::routes())
        .merge(internal::routes())
        .fallback(no_route)
        .method_not_allowed_fallback(no_method)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD))
        .with_state(state)
}

/// Cloudflare's answer for a path it does not route.
async fn no_route(uri: axum::http::Uri) -> ApiError {
    ApiError::new(
        http::StatusCode::NOT_FOUND,
        7003,
        format!("Could not route to {}, perhaps your object identifier is invalid?", uri.path()),
    )
}

async fn no_method(method: axum::http::Method, uri: axum::http::Uri) -> ApiError {
    ApiError::new(
        http::StatusCode::METHOD_NOT_ALLOWED,
        10405,
        format!("Method {method} is not supported for {}", uri.path()),
    )
}

/// The caller, from `Authorization: Bearer <token>`.
pub struct Caller(pub Principal);

impl FromRequestParts<State> for Caller {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &State) -> ApiResult<Self> {
        let token = parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;
        state.db.verify_token(token.trim()).await?.map(Caller).ok_or_else(ApiError::unauthorized)
    }
}

/// Resolves `{account}` and checks the caller may use it.
pub async fn account_access(state: &State, caller: &Caller, account: &str) -> ApiResult<AccountId> {
    let id: AccountId = account.parse().map_err(|_| ApiError::not_found("Account not found"))?;
    if !caller.0.may_access(&id) {
        return Err(ApiError::forbidden());
    }
    if state.db.account(&id).await?.is_none() {
        return Err(ApiError::not_found("Account not found"));
    }
    Ok(id)
}

/// An internal caller (agent or gateway), from the shared internal token.
pub struct Internal;

impl FromRequestParts<State> for Internal {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &State) -> ApiResult<Self> {
        let ok = parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| crate::constant_time_eq(t.as_bytes(), state.internal_token.as_bytes()));
        if ok { Ok(Internal) } else { Err(ApiError::unauthorized()) }
    }
}

pub type AccountPath = Path<String>;

/// A JSON body, whatever its `Content-Type`: clients (wrangler among them)
/// do not always send one.
pub struct JsonBody<T>(pub T);

impl<T: serde::de::DeserializeOwned, S: Send + Sync> axum::extract::FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;
    async fn from_request(req: axum::extract::Request, state: &S) -> ApiResult<Self> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|e| ApiError::bad_request(format!("reading the request body: {e}")))?;
        serde_json::from_slice(&bytes)
            .map(JsonBody)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
    }
}
