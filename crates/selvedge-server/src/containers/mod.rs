//! The containers controller (RFC 0002): starts and stops container
//! instances for runners.
//!
//! Runners execute tenants' code, so they hold no credentials for the
//! cluster. A runner's agent asks this component for an instance of a
//! Durable Object class's container and says only what workerd told it:
//! which class, the environment and the command. Everything else comes from
//! here and from the database: the image, its limits, the sandbox, the pod's
//! shape, the pull credentials. A runner that has been taken over can
//! therefore start nothing but the applications its accounts deployed, within
//! their limits.
//!
//! The controller keeps no state of its own. Instances are pods (or, on a
//! single node, Docker containers) that carry their account, application,
//! session and runner as labels, and every answer is derived from a listing
//! of them.

pub mod docker;
pub mod exec;
pub mod kube;
pub mod spec;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::{Path, Query, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use selvedge_core::AccountId;
use selvedge_core::containers::Resources;
use selvedge_store::Db;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::registry::{Credentials, Permissions};

/// What a runner's agent sends to start an instance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartRequest {
    pub account: AccountId,
    /// The Durable Object class the container belongs to.
    pub script: String,
    pub class: String,
    /// workerd's name for the container: unique per session and object.
    pub name: String,
    /// The account process the container belongs to. When the process ends,
    /// the agent removes the session's instances.
    pub session: String,
    /// `KEY=value`, as `ctx.container.start()` was given them.
    #[serde(default)]
    pub env: Vec<String>,
    /// Replaces the image's command (`entrypoint` of `start()`).
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// The port of the agent's egress relay, which the instance's sidecar
    /// sends the container's outbound connections to.
    pub egress_port: u16,
    /// The sidecar's egress configuration (`PUT /egress`), applied before
    /// the application starts.
    #[serde(default)]
    pub egress: Value,
    /// The runner: its pod's name on Kubernetes.
    pub runner: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Created; the application has not started yet.
    Pending,
    Running,
    /// The application exited.
    Exited,
    /// The instance cannot start (the image cannot be pulled, ...).
    Failed,
    /// No such instance.
    Gone,
}

/// An instance as a backend lists it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub account: String,
    pub application: String,
    pub session: String,
    pub runner: String,
    pub phase: Phase,
    /// Where the sidecar's ingress port is reachable, once running.
    #[serde(default)]
    pub ip: Option<IpAddr>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub message: Option<String>,
    /// Being deleted.
    #[serde(default)]
    pub terminating: bool,
    /// Seconds since it was created.
    #[serde(default)]
    pub age_secs: u64,
}

/// A runner as a backend knows it.
#[derive(Clone, Debug)]
pub struct Runner {
    pub name: String,
    /// What the instance's sidecar reaches the agent's egress relay at.
    pub gateway: String,
    /// Kubernetes: the pod's UID, for the instance's owner reference.
    pub uid: Option<String>,
}

/// Everything a backend needs to create an instance. Built by the
/// controller; nothing here is taken from a runner unchecked except the
/// environment and the command of the tenant's own container.
#[derive(Clone, Debug)]
pub struct InstanceSpec {
    pub id: String,
    pub account: String,
    pub application: String,
    pub session: String,
    pub runner: Runner,
    /// workerd's name for the container, kept as an annotation.
    pub name: String,
    pub image: String,
    pub resources: Resources,
    pub env: Vec<String>,
    pub command: Option<Vec<String>>,
    pub egress_port: u16,
    pub egress: Value,
    /// For the image pull: `(registry host, user, password)`.
    pub pull: (String, String, String),
}

#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    /// Every instance this installation manages.
    async fn list(&self) -> Result<Vec<Instance>>;
    /// Creates an instance; one with the same id already existing is fine.
    async fn create(&self, spec: &InstanceSpec) -> Result<()>;
    /// Deletes an instance, giving its application `grace` seconds to stop.
    /// Deleting one that does not exist is fine.
    async fn delete(&self, id: &str, grace: u32) -> Result<()>;
    /// The runner a request comes from. `peer` is the caller's address when
    /// known: a backend that can check it against the runner's must.
    async fn runner(&self, name: &str, peer: Option<IpAddr>) -> Result<Option<Runner>>;
    /// Runners that are gone for good, whose instances the janitor removes
    /// (`None`: the platform removes them itself, or cannot tell).
    async fn dead_runners(&self, _names: &[String]) -> Result<Vec<String>> {
        Ok(vec![])
    }
    /// Runs a command in a running instance's application container and
    /// answers its exit code. Dropping `io.output` ends the output.
    async fn exec(&self, id: &str, spec: &exec::ExecSpec, io: exec::ExecIo) -> Result<i32>;
}

/// Limits and names the controller applies to every instance.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The registry's host, as image references name it.
    pub registry_host: String,
    /// Most instances an account may run at once, across its applications.
    pub account_instances: usize,
    /// Most instances in the installation (0: no limit).
    pub total_instances: usize,
}

pub struct Controller {
    db: Db,
    backend: Arc<dyn Backend>,
    settings: Settings,
    credentials: Credentials,
    internal_token: String,
    /// The last listing, and a signal for waiters when it changes.
    cache: tokio::sync::watch::Sender<Arc<HashMap<String, Instance>>>,
    /// Asks the refresher for a listing now.
    refresh: tokio::sync::Notify,
    /// Requests waiting on an instance, which make the refresher poll faster.
    waiters: std::sync::atomic::AtomicUsize,
}

/// How often instances are listed while nothing waits on one, and while
/// something does.
const IDLE_REFRESH: Duration = Duration::from_secs(5);
const BUSY_REFRESH: Duration = Duration::from_millis(300);
/// How long a finished instance is kept for its exit code to be read.
const FINISHED_TTL: u64 = 300;
/// How long an instance may stay pending before the janitor removes it.
const PENDING_TTL: u64 = 900;
/// Pull credentials outlive a slow pull and its retries.
const PULL_CREDENTIAL_TTL: u64 = 6 * 3600;

/// The instance id for workerd's container `name` in `session`: also the
/// pod's name, so a DNS label.
pub fn instance_id(account: &AccountId, session: &str, name: &str) -> String {
    let digest = selvedge_core::sha256_hex(format!("{account}\0{session}\0{name}").as_bytes());
    format!("slvc-{}", &digest[..32])
}

#[derive(Debug)]
pub enum StartError {
    /// The class has no application.
    NoApplication,
    /// A limit is reached; the message says which.
    Limit(String),
    /// The caller is not the runner it claims to be.
    Forbidden(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for StartError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl Controller {
    pub fn new(db: Db, backend: Arc<dyn Backend>, settings: Settings, internal_token: String) -> Arc<Self> {
        let (cache, _) = tokio::sync::watch::channel(Arc::new(HashMap::new()));
        Arc::new(Self {
            db,
            backend,
            settings,
            credentials: Credentials::new(&internal_token),
            internal_token,
            cache,
            refresh: tokio::sync::Notify::new(),
            waiters: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Lists instances into the cache, forever, and cleans up after runners
    /// and instances that are gone.
    pub async fn run(self: Arc<Self>) {
        let mut last_janitor = Instant::now();
        loop {
            match self.backend.list().await {
                Ok(list) => {
                    crate::metrics::CONTAINER_INSTANCES
                        .set(&["running"], list.iter().filter(|i| i.phase == Phase::Running).count() as i64);
                    crate::metrics::CONTAINER_INSTANCES
                        .set(&["pending"], list.iter().filter(|i| i.phase == Phase::Pending).count() as i64);
                    if last_janitor.elapsed() >= Duration::from_secs(30) {
                        last_janitor = Instant::now();
                        self.janitor(&list).await;
                    }
                    self.cache.send_replace(Arc::new(list.into_iter().map(|i| (i.id.clone(), i)).collect()));
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "listing container instances failed"),
            }
            let busy = self.waiters.load(std::sync::atomic::Ordering::Relaxed) > 0;
            let wait = if busy { BUSY_REFRESH } else { IDLE_REFRESH };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = self.refresh.notified() => {}
            }
        }
    }

    /// Removes what nothing will remove otherwise: instances of runners that
    /// are gone, finished instances nobody collected, and instances that
    /// never started.
    async fn janitor(&self, list: &[Instance]) {
        let mut runners: Vec<String> = list.iter().map(|i| i.runner.clone()).collect();
        runners.sort();
        runners.dedup();
        let dead = match self.backend.dead_runners(&runners).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "checking runners failed");
                vec![]
            }
        };
        for i in list.iter().filter(|i| !i.terminating) {
            let reason = if dead.contains(&i.runner) {
                "orphaned"
            } else if matches!(i.phase, Phase::Exited | Phase::Failed) && i.age_secs > FINISHED_TTL {
                "finished"
            } else if i.phase == Phase::Pending && i.age_secs > PENDING_TTL {
                "stuck"
            } else {
                continue;
            };
            tracing::info!(instance = %i.id, runner = %i.runner, reason, "removing a container instance");
            crate::metrics::CONTAINER_STOPS.inc(&[reason]);
            if let Err(e) = self.backend.delete(&i.id, 0).await {
                tracing::warn!(instance = %i.id, error = %format!("{e:#}"), "removing a container instance failed");
            }
        }
    }

    async fn fresh(&self) -> Result<Vec<Instance>> {
        let list = self.backend.list().await?;
        self.cache.send_replace(Arc::new(list.iter().map(|i| (i.id.clone(), i.clone())).collect()));
        Ok(list)
    }

    /// Starts an instance for a Durable Object's container. Returns its id
    /// at once; [`Controller::wait`] reports when it runs.
    pub async fn start(&self, req: &StartRequest, peer: Option<IpAddr>) -> Result<String, StartError> {
        let Some(runner) = self.backend.runner(&req.runner, peer).await? else {
            return Err(StartError::Forbidden(format!("`{}` is not a runner at this address", req.runner)));
        };
        let Some(app) = self.db.container_app_of_class(&req.account, &req.script, &req.class).await? else {
            return Err(StartError::NoApplication);
        };
        let id = instance_id(&req.account, &req.session, &req.name);
        let spec = InstanceSpec {
            id: id.clone(),
            account: req.account.to_string(),
            application: app.id.clone(),
            session: req.session.clone(),
            runner,
            name: req.name.clone(),
            image: app.image.clone(),
            resources: app.resources,
            env: req.env.clone(),
            command: req.command.clone(),
            egress_port: req.egress_port,
            egress: req.egress.clone(),
            pull: (
                self.settings.registry_host.clone(),
                crate::registry::USERNAME.to_owned(),
                self.credentials.issue(
                    &req.account,
                    Permissions::PULL,
                    crate::registry::now() + PULL_CREDENTIAL_TTL,
                ),
            ),
        };
        // Counting and creating under one lock: two starts cannot both pass
        // a limit, whichever replicas they reach.
        let limits = self.db.with_container_start_lock(&req.account, async {
            let list = self.fresh().await?;
            if list.iter().any(|i| i.id == id && !i.terminating) {
                return Ok(None);
            }
            // Instances on their way out still hold their resources.
            let live = |i: &&Instance| !matches!(i.phase, Phase::Exited | Phase::Failed);
            let of_app = list.iter().filter(live).filter(|i| i.application == app.id).count();
            let of_account = list.iter().filter(live).filter(|i| i.account == spec.account).count();
            let total = list.iter().filter(live).count();
            if of_app >= app.max_instances.max(0) as usize {
                return Ok(Some(format!(
                    "the application `{}` already runs its maximum of {} instances (max_instances)",
                    app.name, app.max_instances
                )));
            }
            if of_account >= self.settings.account_instances {
                return Ok(Some(format!(
                    "the account already runs its maximum of {} container instances",
                    self.settings.account_instances
                )));
            }
            if self.settings.total_instances > 0 && total >= self.settings.total_instances {
                return Ok(Some("this installation is out of container capacity".to_owned()));
            }
            self.backend.create(&spec).await?;
            anyhow::Ok(None)
        });
        match limits.await?? {
            Some(message) => {
                crate::metrics::CONTAINER_STARTS.inc(&["limit"]);
                Err(StartError::Limit(message))
            }
            None => {
                crate::metrics::CONTAINER_STARTS.inc(&["created"]);
                self.refresh.notify_one();
                Ok(id)
            }
        }
    }

    /// The instance, after waiting up to `timeout` for `until` to hold.
    pub async fn wait(&self, id: &str, until: Until, timeout: Duration) -> Instance {
        let gone = || Instance {
            id: id.to_owned(),
            account: String::new(),
            application: String::new(),
            session: String::new(),
            runner: String::new(),
            phase: Phase::Gone,
            ip: None,
            exit_code: None,
            message: None,
            terminating: false,
            age_secs: 0,
        };
        struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.waiters.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _waiting = Waiting(&self.waiters);
        let mut rx = self.cache.subscribe();
        // The cache may predate the request: look once before trusting it.
        self.refresh.notify_one();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut seen_fresh = false;
        loop {
            let current = rx.borrow_and_update().get(id).cloned();
            let done = match (&current, until) {
                (None, _) => seen_fresh,
                (Some(_), Until::Now) => true,
                (Some(i), Until::Started) => i.phase != Phase::Pending,
                (Some(i), Until::Exited) => matches!(i.phase, Phase::Exited | Phase::Failed),
                (Some(_), Until::Removed) => false,
            };
            if done {
                return current.unwrap_or_else(gone);
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => seen_fresh = true,
                _ => return current.unwrap_or_else(gone),
            }
        }
    }

    pub async fn delete(&self, id: &str, grace: u32, reason: &'static str) -> Result<()> {
        crate::metrics::CONTAINER_STOPS.inc(&[reason]);
        self.backend.delete(id, grace).await?;
        self.refresh.notify_one();
        Ok(())
    }

    /// Deletes a runner's instances: those of one session, or all of them.
    pub async fn delete_matching(&self, session: Option<&str>, runner: &str) -> Result<usize> {
        let list = self.fresh().await?;
        let mut n = 0;
        for i in list.iter().filter(|i| i.runner == runner && session.is_none_or(|s| s == i.session)) {
            self.backend.delete(&i.id, 0).await?;
            crate::metrics::CONTAINER_STOPS.inc(&["session"]);
            n += 1;
        }
        self.refresh.notify_one();
        Ok(n)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Until {
    Now,
    /// Running, or failed to start.
    Started,
    Exited,
    Removed,
}

// ---- HTTP ----

type State = Arc<Controller>;

pub fn router(controller: Arc<Controller>) -> Router {
    Router::new()
        .route("/internal/v1/containers", post(start).delete(delete_matching))
        .route("/internal/v1/containers/{id}", get(status).delete(delete))
        .route("/internal/v1/containers/{id}/exec", post(exec_handler))
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(|r, n| crate::metrics::layer("containers", r, n)))
        .with_state(controller)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn authorized(state: &State, headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| crate::constant_time_eq(t.as_bytes(), state.internal_token.as_bytes()))
}

/// The caller's address, when the listener records it (mTLS).
fn peer(extensions: &http::Extensions) -> Option<IpAddr> {
    extensions.get::<axum::extract::ConnectInfo<crate::tls::Peer>>().map(|c| c.0.addr.ip())
}

async fn start(
    AxState(state): AxState<State>,
    headers: http::HeaderMap,
    extensions: http::Extensions,
    Json(req): Json<StartRequest>,
) -> Response {
    if !authorized(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match state.start(&req, peer(&extensions)).await {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(StartError::NoApplication) => {
            error(StatusCode::NOT_FOUND, "the Durable Object class has no container application")
        }
        Err(StartError::Limit(m)) => error(StatusCode::TOO_MANY_REQUESTS, m),
        Err(StartError::Forbidden(m)) => error(StatusCode::FORBIDDEN, m),
        Err(StartError::Internal(e)) => {
            tracing::error!(error = %format!("{e:#}"), "starting a container instance failed");
            crate::metrics::CONTAINER_STARTS.inc(&["error"]);
            error(StatusCode::INTERNAL_SERVER_ERROR, "starting the container failed")
        }
    }
}

#[derive(Deserialize)]
struct StatusQuery {
    until: Option<Until>,
    timeout_ms: Option<u64>,
    runner: Option<String>,
}

/// Whether the request may act on the instance: it must come from the
/// instance's own runner.
async fn owns(state: &State, instance: &Instance, runner: Option<&str>, peer: Option<IpAddr>) -> bool {
    if instance.phase == Phase::Gone {
        return true;
    }
    let Some(name) = runner else { return false };
    name == instance.runner && matches!(state.backend.runner(name, peer).await, Ok(Some(_)))
}

async fn status(
    AxState(state): AxState<State>,
    headers: http::HeaderMap,
    extensions: http::Extensions,
    Path(id): Path<String>,
    Query(q): Query<StatusQuery>,
) -> Response {
    if !authorized(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let timeout = Duration::from_millis(q.timeout_ms.unwrap_or(0).min(60_000));
    let instance = state.wait(&id, q.until.unwrap_or(Until::Now), timeout).await;
    if !owns(&state, &instance, q.runner.as_deref(), peer(&extensions)).await {
        return error(StatusCode::FORBIDDEN, "not this runner's instance");
    }
    Json(instance).into_response()
}

#[derive(Deserialize)]
struct DeleteQuery {
    grace: Option<u32>,
    reason: Option<String>,
    runner: Option<String>,
}

async fn delete(
    AxState(state): AxState<State>,
    headers: http::HeaderMap,
    extensions: http::Extensions,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Response {
    if !authorized(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let instance = state.wait(&id, Until::Now, Duration::from_secs(5)).await;
    if !owns(&state, &instance, q.runner.as_deref(), peer(&extensions)).await {
        return error(StatusCode::FORBIDDEN, "not this runner's instance");
    }
    let reason = match q.reason.as_deref() {
        Some("exited") => "exited",
        Some("failed") => "failed",
        Some("signal") => "signal",
        _ => "destroyed",
    };
    match state.delete(&id, q.grace.unwrap_or(0).min(300), reason).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(instance = %id, error = %format!("{e:#}"), "deleting a container instance failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "deleting the container failed")
        }
    }
}

#[derive(Deserialize)]
struct ExecQuery {
    runner: Option<String>,
}

/// Runs a command in an instance over an upgraded connection (see
/// [`exec`]): input frames in, output frames and then the exit out.
async fn exec_handler(
    AxState(state): AxState<State>,
    headers: http::HeaderMap,
    extensions: http::Extensions,
    Path(id): Path<String>,
    Query(q): Query<ExecQuery>,
    mut req: axum::extract::Request,
) -> Response {
    use base64::Engine;
    if !authorized(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let instance = state.wait(&id, Until::Now, Duration::from_secs(5)).await;
    if instance.phase != Phase::Running
        || !owns(&state, &instance, q.runner.as_deref(), peer(&extensions)).await
    {
        return error(StatusCode::CONFLICT, "not a running instance of this runner");
    }
    let spec: Option<exec::ExecSpec> = headers
        .get(exec::SPEC_HEADER)
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v.as_bytes()).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());
    let Some(spec) = spec.filter(|s| !s.cmd.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "no command");
    };
    let upgrade = hyper::upgrade::on(&mut req);
    let backend = state.backend.clone();
    tokio::spawn(async move {
        let stream = match upgrade.await {
            Ok(u) => hyper_util::rt::TokioIo::new(u),
            Err(e) => {
                tracing::debug!(error = %e, "an exec connection was not upgraded");
                return;
            }
        };
        run_exec(&*backend, &id, &spec, stream).await;
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::CONNECTION, "upgrade")
        .header(http::header::UPGRADE, exec::PROTOCOL)
        .body(axum::body::Body::empty())
        .expect("a valid response")
}

async fn run_exec<S>(backend: &dyn Backend, id: &str, spec: &exec::ExecSpec, stream: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    use tokio::io::AsyncWriteExt;
    let (mut r, mut w) = tokio::io::split(stream);
    let (in_tx, in_rx) = tokio::sync::mpsc::channel(16);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
    let reader = tokio::spawn(async move {
        while let Ok(Some((kind, payload))) = exec::read_frame(&mut r).await {
            if let Some(input) = exec::input_of(kind, payload) {
                if in_tx.send(input).await.is_err() {
                    return;
                }
            }
        }
        // The agent went away: nothing more will come in.
        let _ = in_tx.send(exec::Input::CloseStdin).await;
    });
    let began = Instant::now();
    tracing::debug!(instance = %id, cmd = ?spec.cmd, tty = spec.tty, stdin = spec.stdin, "exec started");
    let run = backend.exec(id, spec, exec::ExecIo { input: in_rx, output: out_tx });
    let forward = async {
        while let Some(o) = out_rx.recv().await {
            let (kind, payload) = match o {
                exec::Output::Stdout(b) => (exec::STDOUT, b),
                exec::Output::Stderr(b) => (exec::STDERR, b),
            };
            if exec::write_frame(&mut w, kind, &payload).await.is_err() {
                // Keep draining, so the backend is never blocked on output.
                while out_rx.recv().await.is_some() {}
                break;
            }
        }
    };
    let (result, ()) = tokio::join!(run, forward);
    tracing::debug!(instance = %id, result = ?result.as_ref().map_err(|e| format!("{e:#}")), took = ?began.elapsed(), "exec ended");
    let exit = match result {
        Ok(code) => exec::Exit { exit_code: Some(code), error: None },
        Err(e) => {
            tracing::warn!(instance = %id, error = %format!("{e:#}"), "an exec failed");
            exec::Exit { exit_code: None, error: Some(format!("{e:#}")) }
        }
    };
    let _ = exec::write_frame(&mut w, exec::EXIT, &serde_json::to_vec(&exit).unwrap_or_default()).await;
    let _ = w.shutdown().await;
    reader.abort();
}

#[derive(Deserialize)]
struct MatchQuery {
    session: Option<String>,
    runner: Option<String>,
}

async fn delete_matching(
    AxState(state): AxState<State>,
    headers: http::HeaderMap,
    extensions: http::Extensions,
    Query(q): Query<MatchQuery>,
) -> Response {
    if !authorized(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let Some(runner) = q.runner.as_deref() else {
        return error(StatusCode::BAD_REQUEST, "`runner` is required");
    };
    if !matches!(state.backend.runner(runner, peer(&extensions)).await, Ok(Some(_))) {
        return error(StatusCode::FORBIDDEN, "not a runner at this address");
    }
    match state.delete_matching(q.session.as_deref(), runner).await {
        Ok(n) => Json(json!({ "deleted": n })).into_response(),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "deleting container instances failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "deleting the containers failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_are_dns_labels() {
        let account: AccountId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        let id = instance_id(&account, "s", "workerd-s-do-abc-def");
        assert_eq!(id.len(), 37);
        assert!(id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
        assert_ne!(id, instance_id(&account, "t", "workerd-s-do-abc-def"));
    }
}
