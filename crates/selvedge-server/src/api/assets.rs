//! Workers Static Assets uploads, as `wrangler deploy` drives them:
//!
//! 1. `POST .../scripts/:name/assets-upload-session` with `{manifest}` answers
//!    an upload token and the hashes still missing, in buckets.
//! 2. `POST .../workers/assets/upload?base64=true`, authenticated with the
//!    upload token, sends one bucket as multipart (field name = hash, value =
//!    base64). The call that sends the last missing file gets the completion
//!    token.
//! 3. The script upload names the completion token in `metadata.assets.jwt`.
//!
//! Tokens are JWT-shaped, because wrangler reads `exp` from them, but carry no
//! claims Selvedge trusts: they are random and looked up by SHA-256.

use axum::Router;
use axum::extract::{Multipart, Path, Query, State as AxState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use bytes::Bytes;
use rand::RngCore as _;
use selvedge_core::AccountId;
use selvedge_core::assets::{ScriptAssets, UploadManifest, validate_manifest};
use selvedge_core::envelope::Envelope;
use selvedge_store::assets::{AssetFile, SESSION_TTL_SECS, asset_path};
use serde::Deserialize;
use serde_json::json;

use super::scripts::script_access;
use super::{Caller, JsonBody, State};
use crate::error::{ApiError, ApiResult, ok};

/// Upper bound on the raw bytes of one bucket: its base64 form must fit the
/// API's body limit.
const MAX_BUCKET_BYTES: u64 = 40 << 20;
const MAX_BUCKET_FILES: usize = 2000;

pub fn routes() -> Router<State> {
    Router::new()
        .route(
            "/client/v4/accounts/{account}/workers/scripts/{script}/assets-upload-session",
            post(start_session),
        )
        .route("/client/v4/accounts/{account}/workers/assets/upload", post(upload))
}

fn new_token() -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
        + SESSION_TTL_SECS;
    let mut nonce = [0u8; 32];
    rand::rng().fill_bytes(&mut nonce);
    format!(
        "{}.{}.{}",
        b64.encode(br#"{"alg":"none","typ":"JWT"}"#),
        b64.encode(json!({ "exp": exp }).to_string()),
        b64.encode(nonce)
    )
}

fn token_sha(token: &str) -> String {
    selvedge_core::sha256_hex(token.as_bytes())
}

#[derive(Deserialize)]
struct SessionBody {
    manifest: UploadManifest,
}

async fn start_session(
    AxState(state): AxState<State>,
    caller: Caller,
    Path((account, script)): Path<(String, String)>,
    JsonBody(body): JsonBody<SessionBody>,
) -> ApiResult<Response> {
    let id = script_access(&state, &caller, &account, &script).await?;
    validate_manifest(&body.manifest).map_err(ApiError::bad_request)?;
    let mut sizes = std::collections::BTreeMap::new();
    for e in body.manifest.values() {
        sizes.insert(e.hash.clone(), e.size);
    }
    let hashes: Vec<String> = sizes.keys().cloned().collect();
    let missing = state.db.missing_assets(&id, &hashes).await?;

    let mut buckets: Vec<Vec<String>> = Vec::new();
    let mut size = 0;
    for h in &missing {
        let s = sizes[h];
        match buckets.last_mut() {
            Some(b) if b.len() < MAX_BUCKET_FILES && size + s <= MAX_BUCKET_BYTES => {
                b.push(h.clone());
                size += s;
            }
            _ => {
                buckets.push(vec![h.clone()]);
                size = s;
            }
        }
    }
    let token = new_token();
    let sha = token_sha(&token);
    // With nothing to upload, the upload token is the completion token.
    let completion = missing.is_empty().then_some(sha.as_str());
    state.db.create_asset_session(&id, &script, &body.manifest, &missing, &sha, completion).await?;
    Ok(ok(json!({ "jwt": token, "buckets": buckets })))
}

#[derive(Deserialize)]
struct UploadQuery {
    #[serde(default)]
    base64: Option<bool>,
}

async fn upload(
    AxState(state): AxState<State>,
    Path(account): Path<String>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    mut form: Multipart,
) -> ApiResult<Response> {
    let token = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(ApiError::unauthorized)?;
    let id: AccountId = account.parse().map_err(|_| ApiError::unauthorized())?;
    let session =
        state.db.asset_session_by_upload(&id, &token_sha(token)).await?.ok_or_else(ApiError::unauthorized)?;
    let sizes: std::collections::BTreeMap<&str, u64> =
        session.manifest.values().map(|e| (e.hash.as_str(), e.size)).collect();

    let mut files = Vec::new();
    while let Some(field) = form.next_field().await.map_err(|e| ApiError::bad_request(e.to_string()))? {
        let hash = field.name().unwrap_or_default().to_owned();
        let Some(&size) = sizes.get(hash.as_str()) else {
            return Err(ApiError::bad_request(format!("`{hash}` is not in this upload's manifest")));
        };
        let content_type =
            field.content_type().map(str::to_owned).filter(|c| c != "application/null" && !c.is_empty());
        let raw = field.bytes().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
        let bytes = if q.base64.unwrap_or(false) {
            let text: Vec<u8> = raw.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
            Bytes::from(
                base64::engine::general_purpose::STANDARD
                    .decode(&text)
                    .map_err(|_| ApiError::bad_request(format!("`{hash}` is not valid base64")))?,
            )
        } else {
            raw
        };
        if bytes.len() as u64 != size {
            return Err(ApiError::bad_request(format!(
                "`{hash}` is {} bytes, but the manifest says {size}",
                bytes.len()
            )));
        }
        state.blobs.put_at(&asset_path(&id, &hash), bytes).await?;
        files.push(AssetFile { hash, content_type, size: size as i64 });
    }
    state.db.put_asset_files(&id, &files).await?;
    let hashes: Vec<String> = files.into_iter().map(|f| f.hash).collect();
    let completion = new_token();
    if state.db.finish_asset_uploads(&session.id, &hashes, &token_sha(&completion)).await? {
        return Ok(
            (StatusCode::CREATED, axum::Json(Envelope::ok(json!({ "jwt": completion })))).into_response()
        );
    }
    Ok((StatusCode::ACCEPTED, axum::Json(Envelope::ok(json!({})))).into_response())
}

/// Resolves `metadata.assets` of a script upload into a version's assets:
/// checks the completion token, then stores the binary manifest as a blob.
/// The caller has been checked for access to `account`.
pub(super) async fn resolve(
    state: &State,
    account: &AccountId,
    meta: &selvedge_core::model::MetadataAssets,
) -> ApiResult<ScriptAssets> {
    let jwt = meta.jwt.as_deref().ok_or_else(|| ApiError::bad_request("metadata.assets is missing `jwt`"))?;
    let manifest = state
        .db
        .asset_manifest_by_completion(account, &token_sha(jwt))
        .await?
        .ok_or_else(|| ApiError::bad_request("the assets completion token is invalid or has expired"))?;
    let hashes: Vec<String> = manifest.values().map(|e| e.hash.clone()).collect();
    if !state.db.missing_assets(account, &hashes).await?.is_empty() {
        return Err(ApiError::bad_request("some assets of this deployment were not uploaded"));
    }
    let encoded = selvedge_core::assets::encode_manifest(&manifest);
    let sha = state.blobs.put(Bytes::from(encoded)).await?;
    ScriptAssets::from_upload(sha, manifest.len(), &meta.config).map_err(ApiError::bad_request)
}
