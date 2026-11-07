//! Zones and Worker routes, as `wrangler deploy` uses them for `routes`.
//!
//! A zone is a domain the operator assigns to an account (admin token only:
//! Selvedge cannot check who owns a domain, so tenants may not claim one).
//! The account then adds route patterns on it, and the gateway sends
//! matching requests to the Worker.

use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::response::Response;
use axum::routing::get;
use selvedge_core::{AccountId, validate_script_name};
use selvedge_store::Principal;
use selvedge_store::zones::{WorkerRoute, Zone};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, JsonBody, State};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/client/v4/zones", get(list).post(create))
        .route("/client/v4/zones/{zone}", get(describe).delete(remove))
        .route("/client/v4/zones/{zone}/workers/routes", get(list_routes).post(create_route))
        .route(
            "/client/v4/zones/{zone}/workers/routes/{route}",
            get(get_route).put(update_route).delete(delete_route),
        )
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}/routes",
            get(script_routes).put(put_script_routes),
        )
}

async fn script_routes(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = super::scripts::script_access(&state, &caller, &account, &script).await?;
    let routes = state.db.script_routes(&id, &script).await?;
    Ok(ok(json!(routes.iter().map(route_json).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct ScriptRoute {
    pattern: String,
    zone_name: Option<String>,
    zone_id: Option<String>,
    #[serde(default)]
    custom_domain: bool,
}

/// `wrangler deploy` with `routes`: replaces the script's routes.
async fn put_script_routes(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    JsonBody(body): JsonBody<Vec<ScriptRoute>>,
) -> ApiResult<Response> {
    let id = super::scripts::script_access(&state, &caller, &account, &script).await?;
    let zones = state.db.zones(Some(&id), None).await?;
    let mut resolved = Vec::new();
    for r in body.iter().filter(|r| !r.custom_domain) {
        let host = r.pattern.split('/').next().unwrap_or("").trim_start_matches('*').trim_start_matches('.');
        let zone = match (&r.zone_id, &r.zone_name) {
            (Some(z), _) => zones.iter().find(|x| &x.id == z),
            (None, Some(n)) => zones.iter().find(|x| &x.name == n),
            // The longest zone the host falls under.
            (None, None) => zones
                .iter()
                .filter(|x| host == x.name || host.ends_with(&format!(".{}", x.name)))
                .max_by_key(|x| x.name.len()),
        }
        .ok_or_else(|| {
            ApiError::new(
                http::StatusCode::BAD_REQUEST,
                10022,
                format!("No zone of this account covers route `{}`", r.pattern),
            )
        })?;
        check_route(zone, &RouteBody { pattern: r.pattern.clone(), script: Some(script.clone()) })?;
        resolved.push((zone.id.clone(), r.pattern.clone()));
    }
    match state.db.replace_script_routes(&id, &script, &resolved).await? {
        Ok(routes) => Ok(ok(json!(routes.iter().map(route_json).collect::<Vec<_>>()))),
        Err(p) => Err(ApiError::new(
            http::StatusCode::CONFLICT,
            10020,
            format!("Route `{p}` is already assigned to another Worker"),
        )),
    }
}

fn zone_json(z: &Zone) -> Value {
    json!({ "id": z.id, "name": z.name, "status": "active", "type": "full", "account": { "id": z.account_id, "name": "" } })
}

fn route_json(r: &WorkerRoute) -> Value {
    json!({ "id": r.id, "pattern": r.pattern, "script": r.script })
}

fn zone_not_found() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 1001, "Invalid zone identifier")
}

fn valid_zone_name(name: &str) -> bool {
    name.len() <= 253
        && name.contains('.')
        && name.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The zone, if the caller may use it.
async fn zone_access(state: &State, caller: &Caller, zone: &str) -> ApiResult<Zone> {
    let z = state.db.zone(zone).await?.ok_or_else(zone_not_found)?;
    if !caller.0.may_access(&z.account_id) {
        return Err(zone_not_found());
    }
    Ok(z)
}

#[derive(Deserialize)]
struct ListQuery {
    name: Option<String>,
    #[serde(rename = "account.id")]
    account_id: Option<String>,
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> ApiResult<Response> {
    let account: Option<AccountId> = match (&caller.0, &q.account_id) {
        (Principal::Account(a), _) => Some(a.clone()),
        (Principal::Admin, Some(a)) => {
            Some(a.parse().map_err(|_| ApiError::bad_request("invalid account.id"))?)
        }
        (Principal::Admin, None) => None,
    };
    let zones = state.db.zones(account.as_ref(), q.name.as_deref()).await?;
    Ok(ok(json!(zones.iter().map(zone_json).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct CreateZone {
    name: String,
    account: AccountRef,
}

#[derive(Deserialize)]
struct AccountRef {
    id: String,
}

async fn create(
    AxState(state): AxState<State>,
    caller: Caller,
    JsonBody(body): JsonBody<CreateZone>,
) -> ApiResult<Response> {
    if caller.0 != Principal::Admin {
        return Err(ApiError::new(
            http::StatusCode::FORBIDDEN,
            1068,
            "Zones are assigned by the operator (an admin token)",
        ));
    }
    let name = body.name.trim_end_matches('.').to_ascii_lowercase();
    if !valid_zone_name(&name) {
        return Err(ApiError::bad_request(format!("invalid zone name `{}`", body.name)));
    }
    let account: AccountId =
        body.account.id.parse().map_err(|_| ApiError::bad_request("invalid account id"))?;
    if state.db.account(&account).await?.is_none() {
        return Err(ApiError::not_found("Account not found"));
    }
    let z =
        state.db.create_zone(&account, &name).await?.ok_or_else(|| {
            ApiError::new(http::StatusCode::CONFLICT, 1061, format!("{name} already exists"))
        })?;
    Ok(ok(zone_json(&z)))
}

async fn describe(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(zone): Path<String>,
) -> ApiResult<Response> {
    Ok(ok(zone_json(&zone_access(&state, &caller, &zone).await?)))
}

async fn remove(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(zone): Path<String>,
) -> ApiResult<Response> {
    if caller.0 != Principal::Admin {
        return Err(ApiError::forbidden());
    }
    if !state.db.delete_zone(&zone).await? {
        return Err(zone_not_found());
    }
    Ok(ok(json!({ "id": zone })))
}

#[derive(Deserialize)]
struct RouteBody {
    pattern: String,
    script: Option<String>,
}

fn check_route(z: &Zone, body: &RouteBody) -> ApiResult<()> {
    selvedge_core::routes::validate(&body.pattern, &z.name)
        .map_err(|m| ApiError::new(http::StatusCode::BAD_REQUEST, 10022, m))?;
    if let Some(s) = &body.script {
        validate_script_name(s).map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(())
}

async fn list_routes(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(zone): Path<String>,
) -> ApiResult<Response> {
    let z = zone_access(&state, &caller, &zone).await?;
    let routes = state.db.worker_routes(&z.id).await?;
    Ok(ok(json!(routes.iter().map(route_json).collect::<Vec<_>>())))
}

async fn create_route(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(zone): Path<String>,
    JsonBody(body): JsonBody<RouteBody>,
) -> ApiResult<Response> {
    let z = zone_access(&state, &caller, &zone).await?;
    check_route(&z, &body)?;
    let r = state.db.create_worker_route(&z.id, &body.pattern, body.script.as_deref()).await?.ok_or_else(
        || ApiError::new(http::StatusCode::CONFLICT, 10020, "A route with the same pattern already exists"),
    )?;
    Ok(ok(route_json(&r)))
}

async fn get_route(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((zone, route)): Path<(String, String)>,
) -> ApiResult<Response> {
    let z = zone_access(&state, &caller, &zone).await?;
    let r = state
        .db
        .worker_routes(&z.id)
        .await?
        .into_iter()
        .find(|r| r.id == route)
        .ok_or_else(|| ApiError::not_found("Route not found"))?;
    Ok(ok(route_json(&r)))
}

async fn update_route(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((zone, route)): Path<(String, String)>,
    JsonBody(body): JsonBody<RouteBody>,
) -> ApiResult<Response> {
    let z = zone_access(&state, &caller, &zone).await?;
    check_route(&z, &body)?;
    let r = state
        .db
        .update_worker_route(&z.id, &route, &body.pattern, body.script.as_deref())
        .await?
        .ok_or_else(|| ApiError::not_found("Route not found"))?;
    Ok(ok(route_json(&r)))
}

async fn delete_route(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((zone, route)): Path<(String, String)>,
) -> ApiResult<Response> {
    let z = zone_access(&state, &caller, &zone).await?;
    if !state.db.delete_worker_route(&z.id, &route).await? {
        return Err(ApiError::not_found("Route not found"));
    }
    Ok(ok(json!({ "id": route })))
}
