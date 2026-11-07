//! The D1 REST API. Queries run in the account's D1 backend (a system worker
//! with one Durable Object per database), reached through the gateway's
//! internal routing, so they get the same placement and failover as bindings.

use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::response::Response;
use axum::routing::{get, post};
use selvedge_core::AccountId;
use selvedge_store::D1Info;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Caller, JsonBody, State, account_access};
use crate::agent::{ACCOUNT_HEADER, SCRIPT_HEADER, TOKEN_HEADER};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    let base = "/client/v4/accounts/{account}/d1/database";
    Router::new()
        .route(base, get(list).post(create))
        .route(&format!("{base}/{{db}}"), get(get_db).delete(delete_db))
        .route(&format!("{base}/{{db}}/query"), post(query))
        .route(&format!("{base}/{{db}}/raw"), post(raw))
}

fn db_json(d: &D1Info) -> Value {
    json!({
        "uuid": d.id,
        "name": d.name,
        "created_at": d.created_on,
        "version": "production",
        "num_tables": 0,
        "file_size": 0,
        "read_replication": { "mode": "disabled" },
    })
}

fn not_found() -> ApiError {
    ApiError::new(http::StatusCode::NOT_FOUND, 7404, "The database could not be found")
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[derive(Deserialize)]
struct ListQuery {
    name: Option<String>,
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let dbs = state.db.d1_databases(&id).await?;
    let dbs: Vec<_> = dbs
        .iter()
        .filter(|d| q.name.as_ref().is_none_or(|n| d.name.contains(n.as_str())))
        .map(db_json)
        .collect();
    Ok(ok(json!(dbs)))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
}

async fn create(
    AxState(state): AxState<State>,
    caller: Caller,
    Path(account): Path<String>,
    JsonBody(body): JsonBody<CreateBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    if !valid_name(&body.name) {
        return Err(ApiError::bad_request("database names are 1-64 lowercase letters, digits, '-' and '_'"));
    }
    let d = state.db.create_d1(&id, &body.name).await?.ok_or_else(|| {
        ApiError::new(http::StatusCode::CONFLICT, 7502, "A database with that name already exists")
    })?;
    Ok(ok(db_json(&d)))
}

async fn get_db(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, db)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let d = state.db.d1_database(&id, &db).await?.ok_or_else(not_found)?;
    Ok(ok(db_json(&d)))
}

/// Sends a request to the database's Durable Object.
async fn call(state: &State, account: &AccountId, db: &str, path: &str, body: &Value) -> ApiResult<Value> {
    let gateway = state.gateway_url.as_deref().ok_or_else(|| {
        ApiError::new(http::StatusCode::SERVICE_UNAVAILABLE, 7500, "D1 queries need SELVEDGE_GATEWAY_URL")
    })?;
    // The account's process may be starting for the first time.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let resp = state
            .http
            .post(format!("{gateway}{path}"))
            .header(TOKEN_HEADER, &state.internal_token)
            .header(ACCOUNT_HEADER, account.as_str())
            .header(SCRIPT_HEADER, selvedge_workerd::render::D1_SERVICE)
            .header("x-d1-database", db)
            .json(body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                return r.json().await.map_err(|e| anyhow::anyhow!("D1 backend answered badly: {e}").into());
            }
            Ok(r) if r.status().as_u16() == 404 && std::time::Instant::now() < deadline => {}
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("D1 backend: {status}: {text}").into());
            }
            Err(e) if std::time::Instant::now() < deadline => {
                tracing::debug!(error = %e, "D1 call failed; retrying")
            }
            Err(e) => return Err(anyhow::Error::from(e).into()),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn delete_db(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, db)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let d = state.db.d1_database(&id, &db).await?.ok_or_else(not_found)?;
    // Drop the data first; the Durable Object would otherwise keep it forever.
    call(&state, &id, &d.id, "/destroy", &json!({})).await?;
    state.db.delete_d1(&id, &d.id).await?;
    Ok(ok(Value::Null))
}

#[derive(Deserialize)]
struct QueryBody {
    sql: String,
    #[serde(default)]
    params: Vec<Value>,
}

fn query_error(out: &Value) -> Option<ApiError> {
    let failed = out.as_array().and_then(|a| a.first()).unwrap_or(out);
    (failed.get("success") == Some(&json!(false))).then(|| {
        let msg = failed["error"].as_str().unwrap_or("query failed");
        ApiError::new(http::StatusCode::BAD_REQUEST, 7500, format!("{msg}: SQLITE_ERROR"))
    })
}

/// Runs SQL (possibly several statements) in one transaction; the results of
/// the last statement come back as objects.
async fn query(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, db)): Path<(String, String)>,
    JsonBody(body): JsonBody<QueryBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let d = state.db.d1_database(&id, &db).await?.ok_or_else(not_found)?;
    let out = call(&state, &id, &d.id, "/rest", &json!({ "sql": body.sql, "params": body.params })).await?;
    if let Some(e) = query_error(&out) {
        return Err(e);
    }
    Ok(ok(out))
}

/// Like `query`, with rows as arrays and the column names separately.
async fn raw(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, db)): Path<(String, String)>,
    JsonBody(body): JsonBody<QueryBody>,
) -> ApiResult<Response> {
    let id = account_access(&state, &caller, &account).await?;
    let d = state.db.d1_database(&id, &db).await?.ok_or_else(not_found)?;
    let out = call(
        &state,
        &id,
        &d.id,
        "/query?resultsFormat=ROWS_AND_COLUMNS",
        &json!({ "sql": body.sql, "params": body.params }),
    )
    .await?;
    if let Some(e) = query_error(&out) {
        return Err(e);
    }
    Ok(ok(json!([{ "results": out["results"], "success": true, "meta": out["meta"] }])))
}

#[cfg(test)]
mod tests {
    #[test]
    fn names() {
        assert!(super::valid_name("prod-db_1"));
        assert!(!super::valid_name("Prod"));
        assert!(!super::valid_name(""));
    }
}
