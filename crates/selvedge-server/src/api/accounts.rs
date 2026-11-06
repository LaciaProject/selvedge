//! Accounts, tokens and the user endpoints clients call on login.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State as AxState};
use axum::response::Response;
use axum::routing::{get, post};
use selvedge_store::Principal;
use serde::Deserialize;
use serde_json::json;

use super::{Caller, State, account_access};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/client/v4/user", get(user))
        .route("/client/v4/user/tokens/verify", get(verify))
        .route("/client/v4/memberships", get(memberships))
        .route("/client/v4/accounts", get(list).post(create))
        .route("/client/v4/accounts/{account}", get(account))
        .route("/client/v4/accounts/{account}/tokens/verify", get(verify))
        .route("/client/v4/accounts/{account}/selvedge/tokens", post(create_token))
}

async fn user(caller: Caller) -> Response {
    let id = match &caller.0 {
        Principal::Admin => "admin".to_owned(),
        Principal::Account(a) => a.to_string(),
    };
    ok(json!({ "id": id, "email": format!("{id}@selvedge.invalid") }))
}

async fn verify(_caller: Caller) -> Response {
    ok(json!({ "id": "token", "status": "active" }))
}

async fn list(AxState(state): AxState<State>, caller: Caller) -> ApiResult<Response> {
    let accounts = state.db.accounts(&caller.0).await?;
    Ok(ok(json!(
        accounts
            .iter()
            .map(|a| json!({ "id": a.id, "name": a.name, "type": "standard" }))
            .collect::<Vec<_>>()
    )))
}

async fn memberships(AxState(state): AxState<State>, caller: Caller) -> ApiResult<Response> {
    let accounts = state.db.accounts(&caller.0).await?;
    Ok(ok(json!(
        accounts
            .iter()
            .map(|a| json!({
                "id": a.id,
                "account": { "id": a.id, "name": a.name },
                "status": "accepted",
                "roles": ["Administrator"],
            }))
            .collect::<Vec<_>>()
    )))
}

async fn account(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let a = state.db.account(&id).await?.ok_or_else(|| ApiError::not_found("Account not found"))?;
    Ok(ok(json!({ "id": a.id, "name": a.name, "type": "standard" })))
}

#[derive(Deserialize)]
struct CreateAccount {
    name: String,
}

/// Creates an account. Platform admins only.
async fn create(
    AxState(state): AxState<State>,
    caller: Caller,
    Json(body): Json<CreateAccount>,
) -> ApiResult<Response> {
    if caller.0 != Principal::Admin {
        return Err(ApiError::forbidden());
    }
    if body.name.trim().is_empty() || body.name.len() > 255 {
        return Err(ApiError::bad_request("account name must be 1-255 characters"));
    }
    let a = state.db.create_account(body.name.trim()).await?;
    Ok(ok(json!({ "id": a.id, "name": a.name, "type": "standard" })))
}

#[derive(Deserialize)]
struct CreateToken {
    name: String,
}

/// Creates an API token scoped to one account. A Selvedge extension; platform
/// admins only, since an account token can do everything in its account.
async fn create_token(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    Json(body): Json<CreateToken>,
) -> ApiResult<Response> {
    if caller.0 != Principal::Admin {
        return Err(ApiError::forbidden());
    }
    let id = account_access(&state, &caller, &account).await?;
    let token = state.db.create_token(Some(&id), &body.name).await?;
    Ok(ok(json!({ "name": body.name, "value": token })))
}
