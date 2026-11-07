//! `wrangler tail`: tail sessions and their WebSockets.
//!
//! `POST .../scripts/:name/tails` answers a WebSocket URL on this API. The URL
//! carries a random token and is the only credential, as Cloudflare's is; the
//! session ends when its socket closes, on `DELETE`, or when it expires.

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State as AxState};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use rand::RngCore as _;
use selvedge_store::tails::TailSession;
use serde::Deserialize;
use serde_json::{Value, json};

use super::scripts::script_access;
use super::{Caller, Internal, JsonBody, State};
use crate::error::{ApiError, ApiResult, ok};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/client/v4/accounts/{account}/workers/scripts/{script}/tails", post(create).get(list))
        .route("/client/v4/accounts/{account}/workers/scripts/{script}/tails/{id}", delete(remove))
        .route("/tail/v1/{token}", get(socket))
        .route("/internal/v1/accounts/{account}/tail-events", post(ingest))
}

fn session_json(s: &TailSession, url: Option<&str>) -> Value {
    let mut v = json!({ "id": s.id, "expires_at": s.expires_at });
    if let Some(u) = url {
        v["url"] = json!(u);
    }
    v
}

/// The client's address as the load balancer or gateway reports it.
fn client_ip(headers: &HeaderMap) -> Option<String> {
    let first = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    first("cf-connecting-ip").or_else(|| first("x-forwarded-for")).or_else(|| first("x-real-ip"))
}

async fn create(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if state.db.script(&id, &script).await?.is_none() {
        return Err(ApiError::new(
            http::StatusCode::NOT_FOUND,
            10007,
            "This Worker does not exist on your account.",
        ));
    }
    let body: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };
    let filters = match &body["filters"] {
        Value::Null => json!([]),
        f @ Value::Array(_) => f.clone(),
        _ => return Err(ApiError::bad_request("`filters` must be a list")),
    };
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let token = hex::encode(raw);
    let session = state
        .db
        .create_tail(
            &id,
            &script,
            &filters,
            &selvedge_core::sha256_hex(token.as_bytes()),
            client_ip(&headers).as_deref(),
        )
        .await?;
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad_request("missing Host header"))?;
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let url = format!("{}://{host}/tail/v1/{token}", if secure { "wss" } else { "ws" });
    Ok(ok(session_json(&session, Some(&url))))
}

async fn list(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    let sessions = state.db.tails(&id, &script).await?;
    Ok(ok(json!(sessions.iter().map(|s| session_json(s, None)).collect::<Vec<_>>())))
}

async fn remove(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script, tail)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    if !state.db.delete_tail(&id, &script, &tail).await? {
        return Err(ApiError::not_found("Tail not found"));
    }
    Ok(ok(Value::Null))
}

async fn socket(
    AxState(state): AxState<State>,
    Path(token): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let session = state
        .db
        .tail_by_token(&selvedge_core::sha256_hex(token.as_bytes()))
        .await?
        .ok_or_else(|| ApiError::not_found("Tail not found or expired"))?;
    // wrangler sends its one message unmasked, which RFC 6455 forbids for
    // clients but Cloudflare accepts.
    Ok(ws
        .protocols(["trace-v1"])
        .accept_unmasked_frames(true)
        .on_upgrade(move |ws| stream(state, session, ws))
        .into_response())
}

async fn stream(state: State, session: TailSession, mut ws: WebSocket) {
    let mut events = state.tails.subscribe(&session.id);
    let expiry = tokio::time::sleep(std::time::Duration::from_secs(session.expires_in.max(0) as u64));
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(e) => if ws.send(Message::Text(e.into())).await.is_err() { break },
                None => break,
            },
            incoming = ws.recv() => match incoming {
                // wrangler sends `{"debug": ...}` once; nothing else is expected.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            () = &mut expiry => {
                let _ = ws.send(Message::Close(None)).await;
                break;
            }
        }
    }
    // One socket per session, as on Cloudflare: closing it ends the session.
    if let Err(e) = state.db.delete_tail(&session.account, &session.script, &session.id).await {
        tracing::warn!(error = %format!("{e:#}"), "deleting a closed tail session failed");
    }
}

#[derive(Deserialize)]
struct Events {
    script: String,
    events: Vec<Value>,
}

/// Trace events from an agent.
async fn ingest(
    AxState(state): AxState<State>,
    _: Internal,
    Path(account): Path<String>,
    JsonBody(body): JsonBody<Events>,
) -> ApiResult<Response> {
    let id = account.parse().map_err(|_| ApiError::not_found("Account not found"))?;
    state.tails.publish(&id, &body.script, body.events).await?;
    Ok(ok(Value::Null))
}
