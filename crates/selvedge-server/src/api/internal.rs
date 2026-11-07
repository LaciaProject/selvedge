//! Internal endpoints for agents and gateways. Authenticated with the shared
//! internal token; never exposed outside the cluster.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State as AxState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use selvedge_core::AccountId;
use serde_json::json;

use super::{Internal, State};
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/internal/v1/routes", get(routes_table))
        .route("/internal/v1/accounts/{account}/generation", get(generation))
        .route("/internal/v1/generations", get(generations))
        .route("/internal/v1/accounts/{account}/bundle", get(bundle))
        .route("/internal/v1/blobs/{sha}", get(blob))
        .route("/healthz", get(|| async { "ok" }))
}

/// The routing table, with an ETag so gateways can poll cheaply.
async fn routes_table(
    AxState(state): AxState<State>,
    _: Internal,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let mut routes = state.db.routes(state.workers_dev_zone.as_deref()).await?;
    routes.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    let body = serde_json::to_vec(&json!({ "routes": routes })).map_err(anyhow::Error::from)?;
    let etag = format!("\"{}\"", &selvedge_core::sha256_hex(&body)[..32]);
    if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, [("etag", etag)]).into_response());
    }
    Ok(([("etag", etag), ("content-type", "application/json".to_owned())], body).into_response())
}

fn account_id(raw: &str) -> ApiResult<AccountId> {
    raw.parse().map_err(|_| ApiError::not_found("Account not found"))
}

async fn generation(
    AxState(state): AxState<State>,
    _: Internal,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_id(&account)?;
    let generation =
        state.db.generation(&id).await?.ok_or_else(|| ApiError::not_found("Account not found"))?;
    Ok(Json(json!({ "generation": generation })).into_response())
}

#[derive(serde::Deserialize)]
struct After {
    after: Option<i64>,
}

/// Accounts that changed after a cursor: `{"changes": [[account, generation]], "cursor": n}`.
/// Without `after`, only the current cursor.
async fn generations(
    AxState(state): AxState<State>,
    _: Internal,
    axum::extract::Query(q): axum::extract::Query<After>,
) -> ApiResult<Response> {
    let Some(after) = q.after else {
        return Ok(
            Json(json!({ "changes": [], "cursor": state.db.generation_head().await? })).into_response()
        );
    };
    let changes = state.db.generations_after(after, 1000).await?;
    let cursor = changes.last().map_or(after, |(_, g)| *g);
    Ok(Json(json!({ "changes": changes, "cursor": cursor })).into_response())
}

async fn bundle(
    AxState(state): AxState<State>,
    _: Internal,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_id(&account)?;
    let bundle = state
        .db
        .bundle(&state.secrets, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Account not found"))?;
    Ok(Json(bundle).into_response())
}

async fn blob(AxState(state): AxState<State>, _: Internal, Path(sha): Path<String>) -> ApiResult<Response> {
    let bytes = state
        .blobs
        .get(&sha)
        .await
        .map_err(|_| ApiError::not_found("Blob not found"))?
        .ok_or_else(|| ApiError::not_found("Blob not found"))?;
    Ok(([("content-type", "application/octet-stream")], bytes).into_response())
}
