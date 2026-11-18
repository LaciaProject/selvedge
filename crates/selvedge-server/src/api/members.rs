//! Who may use an account, and its tokens (RFC 0003). Selvedge extensions
//! under `/selvedge/`, for the dashboard and for scripts with an admin token.
//!
//! - Members: anyone with access sees them; platform admins add and remove.
//! - The account's provider group: platform admins set it.
//! - Tokens: platform admins and the account's users list and revoke them;
//!   an account token does not manage tokens.
//! - Users: platform admins list and remove them.

use axum::Router;
use axum::extract::{Path, State as AxState};
use axum::response::Response;
use axum::routing::{delete, get};
use selvedge_store::Principal;
use serde::Deserialize;
use serde_json::json;

use super::{Caller, JsonBody, State, account_access, require_admin};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/selvedge";
    Router::new()
        .route(&format!("{base}/members"), get(members).post(add_member))
        .route(&format!("{base}/members/{{member}}"), delete(remove_member))
        .route(&format!("{base}/oidc-group"), get(oidc_group).put(set_oidc_group))
        .route(&format!("{base}/tokens"), get(tokens))
        .route(&format!("{base}/tokens/{{token}}"), delete(revoke_token))
        .route("/client/v4/selvedge/users", get(users))
        .route("/client/v4/selvedge/users/{user}", delete(remove_user))
}

async fn members(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    Ok(ok(json!(state.db.members(&id).await?)))
}

/// Adds a user who has logged in before (`user_id`), or invites an email.
#[derive(Deserialize)]
struct AddMember {
    user_id: Option<String>,
    email: Option<String>,
}

async fn add_member(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    JsonBody(body): JsonBody<AddMember>,
) -> ApiResult<Response> {
    require_admin(&caller)?;
    let id = account_access(&state, &caller, &account).await?;
    let added = match (body.user_id.as_deref(), body.email.as_deref().map(str::trim)) {
        (Some(user), None) => state.db.add_member(&id, user).await?.ok_or_else(|| {
            ApiError::new(http::StatusCode::CONFLICT, 1001, "No such user, or a member already")
        })?,
        (None, Some(email)) => {
            if !valid_email(email) {
                return Err(ApiError::bad_request(format!("`{email}` is not an email address")));
            }
            state.db.invite_member(&id, email).await?.ok_or_else(|| {
                ApiError::new(
                    http::StatusCode::CONFLICT,
                    1001,
                    format!("{email} is a member or invited already"),
                )
            })?
        }
        _ => return Err(ApiError::bad_request("give either user_id or email")),
    };
    Ok(ok(json!({ "id": added })))
}

async fn remove_member(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, member)): Path<(String, String)>,
) -> ApiResult<Response> {
    require_admin(&caller)?;
    let id = account_access(&state, &caller, &account).await?;
    if !state.db.remove_member(&id, &member).await? {
        return Err(ApiError::not_found("Member not found"));
    }
    Ok(ok(json!({ "id": member })))
}

async fn oidc_group(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    Ok(ok(json!({ "group": state.db.oidc_group(&id).await? })))
}

#[derive(Deserialize)]
struct OidcGroup {
    group: Option<String>,
}

async fn set_oidc_group(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    JsonBody(body): JsonBody<OidcGroup>,
) -> ApiResult<Response> {
    require_admin(&caller)?;
    let id = account_access(&state, &caller, &account).await?;
    let group = body.group.as_deref().map(str::trim).filter(|g| !g.is_empty());
    if group.is_some_and(|g| g.len() > 255) {
        return Err(ApiError::bad_request("a group name is at most 255 characters"));
    }
    state.db.set_oidc_group(&id, group).await?;
    Ok(ok(json!({ "group": group })))
}

/// Admins and dashboard users manage tokens; account tokens do not.
fn may_manage_tokens(caller: &Caller) -> ApiResult<()> {
    if matches!(caller.0, Principal::Account(_)) { Err(ApiError::forbidden()) } else { Ok(()) }
}

async fn tokens(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
) -> ApiResult<Response> {
    may_manage_tokens(&caller)?;
    let id = account_access(&state, &caller, &account).await?;
    Ok(ok(json!(state.db.tokens(&id).await?)))
}

async fn revoke_token(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, token)): Path<(String, String)>,
) -> ApiResult<Response> {
    may_manage_tokens(&caller)?;
    let id = account_access(&state, &caller, &account).await?;
    if !state.db.delete_token(&id, &token).await? {
        return Err(ApiError::not_found("Token not found"));
    }
    Ok(ok(json!({ "id": token })))
}

async fn users(AxState(state): AxState<State>, caller: Caller) -> ApiResult<Response> {
    require_admin(&caller)?;
    Ok(ok(json!(state.db.users().await?)))
}

async fn remove_user(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(user): Path<String>,
) -> ApiResult<Response> {
    require_admin(&caller)?;
    if !state.db.delete_user(&user).await? {
        return Err(ApiError::not_found("User not found"));
    }
    Ok(ok(json!({ "id": user })))
}

/// Loosely: something@something.something, no spaces, a sane length. The
/// provider is what actually verifies an email.
fn valid_email(e: &str) -> bool {
    let Some((local, domain)) = e.rsplit_once('@') else { return false };
    e.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !e.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    #[test]
    fn emails() {
        for good in ["a@b.co", "first.last+tag@example.com"] {
            assert!(super::valid_email(good), "{good}");
        }
        for bad in ["", "a", "a@b", "@b.co", "a b@c.co", "a@.co", "a@co."] {
            assert!(!super::valid_email(bad), "{bad}");
        }
    }
}
