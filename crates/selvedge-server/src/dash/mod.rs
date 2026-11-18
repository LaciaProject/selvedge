//! The web dashboard's listener (RFC 0003 §5): its own origin, served by the
//! API process. It serves the dashboard's files, logs people in (OpenID
//! Connect, or an API token), and hands `/client/v4` and `/tail/v1` requests
//! to the same handlers as the public listener, with the caller taken from
//! the session cookie instead of a Bearer token.

mod files;
pub mod oidc;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::extract::{Query, Request, State as AxState};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use selvedge_store::Principal;
use selvedge_store::dash::{SessionOwner, SessionStart};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower::ServiceExt;

use crate::api::{DashCaller, State as ApiStateRef};
use crate::error::{ApiError, ApiResult, ok};

pub use oidc::OidcConfig;

pub struct DashConfig {
    /// The dashboard's public URL (`https://dash.example.com`): its origin is
    /// what requests must come from, and OpenID Connect redirects back to it.
    pub url: url::Url,
    /// The built dashboard (`dash/dist`), if installed.
    pub static_dir: Option<std::path::PathBuf>,
    pub token_login: bool,
    pub session_idle: Duration,
    pub session_max: Duration,
    pub oidc: Option<OidcConfig>,
}

pub struct Dash {
    cfg: DashConfig,
    /// `scheme://host[:port]`, as browsers send it in `Origin`.
    origin: String,
    secure: bool,
    oidc: Option<oidc::Provider>,
    api: ApiStateRef,
    /// The API's handlers, as the public listener has them.
    api_router: Router,
}

type State = Arc<Dash>;

/// Marks requests from the dashboard's own code: a cross-site form or tag
/// cannot set it.
const DASH_HEADER: &str = "x-selvedge-dash";
const LOGIN_TTL: Duration = Duration::from_secs(600);

impl Dash {
    pub fn new(cfg: DashConfig, api: ApiStateRef) -> Result<Arc<Self>> {
        let origin = cfg.url.origin().ascii_serialization();
        anyhow::ensure!(origin != "null", "the dashboard URL {} has no origin", cfg.url);
        let secure = cfg.url.scheme() == "https";
        if !secure && !matches!(cfg.url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) {
            tracing::warn!(url = %cfg.url, "the dashboard URL is not https: its cookies are not Secure");
        }
        let oidc = cfg.oidc.clone().map(oidc::Provider::new).transpose()?;
        let api_router = crate::api::router(api.clone());
        Ok(Arc::new(Self { origin, secure, oidc, api, api_router, cfg }))
    }

    fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.origin)
    }

    fn cookie_name(&self, base: &str) -> String {
        // __Host-: Secure, Path=/, no Domain; the browser holds us to it.
        if self.secure { format!("__Host-selvedge-{base}") } else { format!("selvedge-{base}") }
    }

    fn set_cookie(&self, base: &str, value: &str, max_age: Duration) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
            self.cookie_name(base),
            max_age.as_secs()
        ))
        .expect("cookie values are hex or base64url")
    }

    fn cookie<'a>(&self, headers: &'a HeaderMap, base: &str) -> Option<&'a str> {
        let name = self.cookie_name(base);
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|c| c.trim().split_once('='))
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v)
    }

    /// The caller behind the session cookie, if it holds a live session.
    async fn caller(&self, headers: &HeaderMap) -> Result<Option<DashCaller>> {
        let Some(value) = self.cookie(headers, "session") else { return Ok(None) };
        Ok(match self.api.db.session(value, self.cfg.session_idle).await? {
            None => None,
            Some(SessionOwner::Token { principal, .. }) => Some(DashCaller { principal, user: None }),
            Some(SessionOwner::User(user)) => {
                // Memberships are read on each request, so removing one
                // takes effect at once.
                let principal = if user.admin {
                    Principal::Admin
                } else {
                    Principal::User { id: user.id.clone(), accounts: self.api.db.user_accounts(&user).await? }
                };
                Some(DashCaller { principal, user: Some(user) })
            }
        })
    }

    /// Requests that act must come from the dashboard's own pages.
    fn check_origin(&self, headers: &HeaderMap) -> ApiResult<()> {
        match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
            Some(o) if o == self.origin => Ok(()),
            _ => Err(ApiError::new(StatusCode::FORBIDDEN, 10000, "Cross-origin request refused")),
        }
    }

    fn check_request(&self, req: &Request) -> ApiResult<()> {
        if req.headers().get(DASH_HEADER).is_none_or(|v| v != "1") {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                10000,
                format!("Missing the {DASH_HEADER} header"),
            ));
        }
        if !matches!(*req.method(), Method::GET | Method::HEAD) {
            self.check_origin(req.headers())?;
        }
        Ok(())
    }
}

pub fn router(dash: State) -> Router {
    Router::new()
        .route("/auth/config", get(config))
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/token", post(token_login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .fallback(files::serve)
        .layer(axum::middleware::from_fn_with_state(dash.clone(), front))
        .with_state(dash)
}

/// Every request: the API's paths go to the API's handlers with the session's
/// caller, the rest to the routes above; all answers get the security headers.
async fn front(AxState(d): AxState<State>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let resp = if path == "/client/v4" || path.starts_with("/client/v4/") {
        // The only way in here is the session cookie.
        req.headers_mut().remove(header::AUTHORIZATION);
        match d.check_request(&req) {
            Err(e) => e.into_response(),
            Ok(()) => match d.caller(req.headers()).await {
                Err(e) => ApiError::from(e).into_response(),
                Ok(None) => ApiError::unauthorized().into_response(),
                Ok(Some(caller)) => {
                    let method = req.method().clone();
                    let who = caller.user.as_ref().map(|u| (u.id.clone(), u.email.clone()));
                    req.extensions_mut().insert(caller);
                    d.forwarded(&mut req);
                    let resp = api(&d, req).await;
                    // RFC 0003 §4.5: who changed what from the dashboard.
                    if !matches!(method, Method::GET | Method::HEAD) {
                        let (user, email) = who.unzip();
                        tracing::info!(
                            %method, path, status = resp.status().as_u16(), user, email = email.flatten(),
                            "dashboard change"
                        );
                    }
                    resp
                }
            },
        }
    } else if path.starts_with("/d1/v1/transfers/") {
        // D1 import and export files: the path's token is the credential.
        // Downloads are navigations, which send no Origin.
        match req.method() {
            &Method::GET | &Method::HEAD => api(&d, req).await,
            _ => match d.check_origin(req.headers()) {
                Err(e) => e.into_response(),
                Ok(()) => api(&d, req).await,
            },
        }
    } else if path.starts_with("/tail/v1/") {
        // A WebSocket: browsers cannot add headers to it, but they do send
        // its Origin. The path's token is what grants access.
        match d.check_origin(req.headers()) {
            Err(e) => e.into_response(),
            Ok(()) => api(&d, req).await,
        }
    } else {
        if matches!(path.as_str(), "/auth/me" | "/auth/token" | "/auth/logout") {
            if let Err(e) = d.check_request(&req) {
                return with_headers(&d, e.into_response(), true);
            }
        }
        next.run(req).await
    };
    let private = path.starts_with("/client/v4") || path.starts_with("/auth/");
    with_headers(&d, resp, private)
}

impl Dash {
    /// The API builds URLs (the tail WebSocket) from these: make them the
    /// dashboard's, whatever proxies are in between.
    fn forwarded(&self, req: &mut Request) {
        let h = req.headers_mut();
        if let Some(host) = self.cfg.url.host_str() {
            let host = match self.cfg.url.port() {
                Some(p) => format!("{host}:{p}"),
                None => host.to_owned(),
            };
            if let Ok(v) = HeaderValue::from_str(&host) {
                h.insert("x-forwarded-host", v);
            }
        }
        h.insert("x-forwarded-proto", HeaderValue::from_static(if self.secure { "https" } else { "http" }));
    }
}

async fn api(d: &Dash, req: Request) -> Response {
    match d.api_router.clone().oneshot(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    }
}

fn with_headers(d: &Dash, mut resp: Response, private: bool) -> Response {
    let ws = d.origin.replacen("http", "ws", 1);
    let csp = format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
         font-src 'self'; connect-src 'self' {ws}; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; \
         form-action 'self'"
    );
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(&csp).expect("ASCII"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    if private {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

// ---- login ----

async fn config(AxState(d): AxState<State>) -> Response {
    ok(json!({
        "oidc": d.oidc.as_ref().map(|p| json!({ "name": p.cfg.name })),
        "token_login": d.cfg.token_login,
        // For showing Workers' workers.dev-style URLs.
        "workers_dev_zone": d.api.workers_dev_zone,
    }))
}

/// The login in progress, sealed into a short-lived cookie between leaving
/// for the provider and coming back.
#[derive(Serialize, Deserialize)]
struct Pending {
    state: String,
    nonce: String,
    verifier: String,
    return_to: String,
    expires: i64,
}

const LOGIN_AAD: &str = "selvedge-dash-login";

#[derive(Deserialize)]
struct LoginQuery {
    return_to: Option<String>,
}

/// Only a path on this site: never `//elsewhere` or a URL.
fn safe_return(to: Option<&str>) -> String {
    match to {
        Some(p)
            if p.starts_with('/')
                && !p.starts_with("//")
                && !p.contains('\\')
                && !p.starts_with("/auth/") =>
        {
            p.to_owned()
        }
        _ => "/".to_owned(),
    }
}

/// Back to the login page, which explains `error` in the reader's language.
fn login_failed(error: &str) -> Response {
    Redirect::to(&format!("/login?error={error}")).into_response()
}

async fn login(AxState(d): AxState<State>, Query(q): Query<LoginQuery>) -> Response {
    let Some(p) = &d.oidc else { return login_failed("oidc_disabled") };
    let pending = Pending {
        state: oidc::random_token(),
        nonce: oidc::random_token(),
        verifier: oidc::random_token(),
        return_to: safe_return(q.return_to.as_deref()),
        expires: unix_now() + LOGIN_TTL.as_secs() as i64,
    };
    let url =
        match p.authorization_url(&d.redirect_uri(), &pending.state, &pending.nonce, &pending.verifier).await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "OIDC provider unavailable");
                return login_failed("provider_unavailable");
            }
        };
    let sealed = match d.api.secrets.seal(&serde_json::to_vec(&pending).expect("serializable"), LOGIN_AAD) {
        Ok(s) => s,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let cookie = d.set_cookie("login", &base64url(&sealed), LOGIN_TTL);
    ([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(
    AxState(d): AxState<State>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let clear = d.set_cookie("login", "", Duration::ZERO);
    let resp = match finish_login(&d, &headers, &q).await {
        Ok((session, return_to)) => (
            [(header::SET_COOKIE, d.set_cookie("session", &session, d.cfg.session_max))],
            Redirect::to(&return_to),
        )
            .into_response(),
        Err(code) => login_failed(code),
    };
    let mut resp = resp;
    resp.headers_mut().append(header::SET_COOKIE, clear);
    resp
}

async fn finish_login(
    d: &Dash,
    headers: &HeaderMap,
    q: &CallbackQuery,
) -> std::result::Result<(String, String), &'static str> {
    let p = d.oidc.as_ref().ok_or("oidc_disabled")?;
    if let Some(e) = &q.error {
        tracing::warn!(
            error = e,
            description = q.error_description.as_deref(),
            "the OIDC provider refused the login"
        );
        return Err(if e == "access_denied" { "access_denied" } else { "provider_error" });
    }
    let pending: Pending = d
        .cookie(headers, "login")
        .and_then(unbase64url)
        .and_then(|sealed| d.api.secrets.open(&sealed, LOGIN_AAD).ok())
        .and_then(|plain| serde_json::from_slice(&plain).ok())
        .ok_or("login_expired")?;
    if pending.expires < unix_now() {
        return Err("login_expired");
    }
    if q.state.as_deref() != Some(pending.state.as_str()) {
        tracing::warn!("OIDC callback with a state that does not match the login's");
        return Err("login_expired");
    }
    let code = q.code.as_deref().ok_or("provider_error")?;
    let id = p.finish(code, &d.redirect_uri(), &pending.verifier, &pending.nonce).await.map_err(|e| {
        tracing::error!(error = %format!("{e:#}"), "OIDC login failed");
        "login_failed"
    })?;
    let issuer = p.cfg.issuer.as_str();
    let admin = id.is_admin(&p.cfg);
    let user = d
        .api
        .db
        .login_user(&selvedge_store::dash::Login {
            issuer,
            subject: &id.subject,
            email: id.email.as_deref(),
            email_verified: id.email_verified,
            name: id.name.as_deref(),
            admin,
            groups: &id.groups,
        })
        .await
        .map_err(server_error)?;
    let session = d
        .api
        .db
        .create_session(SessionStart::User(&user.id), d.cfg.session_max)
        .await
        .map_err(server_error)?;
    tracing::info!(user = user.id, email = user.email.as_deref(), admin, "dashboard login");
    Ok((session, pending.return_to))
}

fn server_error(e: anyhow::Error) -> &'static str {
    tracing::error!(error = %format!("{e:#}"), "dashboard login failed");
    "server_error"
}

#[derive(Deserialize)]
struct TokenLogin {
    token: String,
}

async fn token_login(
    AxState(d): AxState<State>,
    crate::api::JsonBody(body): crate::api::JsonBody<TokenLogin>,
) -> ApiResult<Response> {
    if !d.cfg.token_login {
        return Err(ApiError::new(StatusCode::FORBIDDEN, 10000, "Logging in with a token is turned off"));
    }
    let token = body.token.trim();
    if d.api.db.verify_token(token).await?.is_none() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, 10000, "Invalid API token"));
    }
    let session = d.api.db.create_session(SessionStart::Token(token), d.cfg.session_max).await?;
    Ok(([(header::SET_COOKIE, d.set_cookie("session", &session, d.cfg.session_max))], ok(json!({})))
        .into_response())
}

async fn logout(AxState(d): AxState<State>, headers: HeaderMap) -> ApiResult<Response> {
    let mut provider_logout = None;
    if let Some(value) = d.cookie(&headers, "session") {
        if let (Some(SessionOwner::User(_)), Some(p)) =
            (d.api.db.session(value, d.cfg.session_idle).await?, &d.oidc)
        {
            provider_logout = p.logout_url(&format!("{}/login", d.origin)).await;
        }
        d.api.db.end_session(value).await?;
    }
    Ok((
        [(header::SET_COOKIE, d.set_cookie("session", "", Duration::ZERO))],
        ok(json!({ "logout_url": provider_logout })),
    )
        .into_response())
}

/// Who is logged in, and which accounts they may use.
async fn me(AxState(d): AxState<State>, headers: HeaderMap) -> ApiResult<Response> {
    let caller = d.caller(&headers).await?.ok_or_else(ApiError::unauthorized)?;
    let accounts = d.api.db.accounts(&caller.principal).await?;
    let token_name = match (&caller.user, d.cookie(&headers, "session")) {
        (None, Some(v)) => match d.api.db.session(v, d.cfg.session_idle).await? {
            Some(SessionOwner::Token { name, .. }) => Some(name),
            _ => None,
        },
        _ => None,
    };
    Ok(ok(json!({
        "user": caller.user,
        "token": token_name.map(|name| json!({ "name": name })),
        "admin": caller.principal == Principal::Admin,
        "accounts": accounts.iter().map(|a| json!({ "id": a.id, "name": a.name })).collect::<Vec<_>>(),
    })))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn base64url(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn unbase64url(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn return_paths_stay_on_the_site() {
        use super::safe_return;
        assert_eq!(safe_return(Some("/workers/x?tab=logs")), "/workers/x?tab=logs");
        for bad in
            ["//evil.example.com", "https://evil.example.com", "/\\evil.example.com", "evil", "/auth/login"]
        {
            assert_eq!(safe_return(Some(bad)), "/", "{bad}");
        }
        assert_eq!(safe_return(None), "/");
    }
}
