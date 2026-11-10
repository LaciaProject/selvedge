//! Containers for account processes (RFC 0002 §4).
//!
//! workerd runs a Durable Object's container through the Docker Engine API.
//! Handing it a Docker socket would hand tenants the node, so each account
//! process gets a socket of the agent's instead, which speaks the part of
//! that API workerd uses and answers everything else with 501. Behind it,
//! an instance is a sandboxed pod that the containers controller creates;
//! the agent never holds credentials for the cluster.
//!
//! workerd's model is two containers: a sidecar that owns the network
//! namespace, created first and kept across restarts of the application, and
//! the application in it. A pod's containers are fixed when it is created,
//! so the sidecar workerd sees is the agent's stand-in: it accepts the
//! sidecar's control calls, remembers the egress configuration, and the real
//! sidecar starts with the application, configured before the application's
//! first instruction runs.
//!
//! Traffic:
//!
//! - **Into the container.** workerd dials `127.0.0.1:<port>`, a listener
//!   the agent opened for that one container, and the agent forwards to the
//!   pod's sidecar. The loopback is shared by every account in the runner,
//!   so the agent checks that the client socket belongs to the account's
//!   user.
//! - **Out of the container.** The sidecar sends everything to the agent's
//!   egress relay. The relay knows which container a connection comes from
//!   by its source address, and forwards to the egress listener workerd
//!   opened for that container, on loopback. A container can therefore reach
//!   only its own Durable Object's egress handling, and nothing else in the
//!   runner.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use http::{Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use selvedge_core::AccountId;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::containers::spec::INGRESS_PORT;
use crate::containers::{Instance, Phase, StartRequest};
use crate::proxy::{self, Body};

/// How the agent runs containers.
#[derive(Clone, Debug)]
pub struct ContainersConfig {
    /// The containers controller.
    pub controller_url: String,
    /// Where the egress relay listens; instances' sidecars must reach it.
    pub egress_listen: SocketAddr,
    /// The sidecar image's name in workerd's configuration. The controller
    /// decides which image actually runs.
    pub interceptor_image: String,
    /// This runner, as the controller knows it (the pod's name).
    pub runner: String,
    /// How long an instance may take to start.
    pub start_timeout: Duration,
}

/// A Durable Object class with a container, as a session's socket knows it.
#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub script: String,
    pub class: String,
    pub image: String,
}

/// What a Docker socket stands for: one account process.
pub struct Session {
    pub account: AccountId,
    pub id: String,
    /// The account's user, when accounts have their own.
    pub uid: Option<u32>,
    /// Classes by their `uniqueKey` in workerd's configuration.
    pub classes: HashMap<String, ClassInfo>,
}

#[derive(Clone, Debug, PartialEq)]
enum AppState {
    Created,
    Running,
    Exited(i32),
}

struct App {
    env: Vec<String>,
    command: Option<Vec<String>>,
    instance: Option<String>,
    state: AppState,
}

/// The stand-in for a container's sidecar.
struct Sidecar {
    /// The loopback port workerd reaches the container through.
    port: u16,
    listener: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct BoxInner {
    sidecar: Option<Sidecar>,
    app: Option<App>,
    /// The pod's address while the application runs.
    ip: Option<IpAddr>,
    /// The last egress configuration workerd set, with the agent's relay as
    /// its port.
    egress: Option<Value>,
    /// The port of workerd's egress listener for this container.
    workerd_egress: Option<u16>,
}

/// One Durable Object's container.
struct ContainerBox {
    session: Arc<Session>,
    class: ClassInfo,
    /// workerd's name for the application container.
    name: String,
    inner: Mutex<BoxInner>,
    /// Serializes lifecycle calls; workerd queues its own, but a monitor's
    /// cleanup and a destroy can meet.
    op: tokio::sync::Mutex<()>,
}

/// An exec workerd created in a container.
struct Exec {
    /// The session and container it belongs to.
    session: String,
    container: String,
    spec: crate::containers::exec::ExecSpec,
    state: Mutex<ExecState>,
}

#[derive(Default)]
struct ExecState {
    started: bool,
    /// What inspect reports as its PID once started. workerd waits for a
    /// non-zero one; the process's real PID is the container's business
    /// (workerd's kill() finds it inside), so this is a counter.
    pid: u32,
    running: bool,
    exit_code: Option<i32>,
    finished: Option<Instant>,
    /// To the controller, while it runs.
    input: Option<tokio::sync::mpsc::Sender<crate::containers::exec::Input>>,
}

/// How long a finished exec stays inspectable.
const EXEC_KEEP: Duration = Duration::from_secs(300);
/// The exit code of an exec that could not run.
const EXEC_FAILED: i32 = 126;

pub struct Shim {
    cfg: ContainersConfig,
    internal_token: String,
    /// To the controller.
    http: reqwest::Client,
    /// To the controller, for execs: upgraded connections that last as long
    /// as the command.
    exec_http: reqwest::Client,
    execs: Mutex<HashMap<String, Arc<Exec>>>,
    exec_pids: std::sync::atomic::AtomicU32,
    /// To instances' sidecars.
    sidecars: reqwest::Client,
    boxes: Mutex<HashMap<String, Arc<ContainerBox>>>,
    sessions: Mutex<HashMap<String, (tokio::task::JoinHandle<()>, PathBuf)>>,
}

/// What workerd reads as the sidecar's CA before an instance exists. The
/// real one is written into the container when it starts.
const PLACEHOLDER_CA: &str = "# The container's CA certificate is installed when the container starts.\n";
/// Exit code reported for a container removed while being monitored.
const KILLED: i32 = 137;
const MAX_BODY: usize = 4 << 20;

fn docker_error(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    let mut r = proxy::full(status, json!({ "message": message.into() }).to_string());
    r.headers_mut().insert("content-type", http::HeaderValue::from_static("application/json"));
    r
}

fn docker_json(status: StatusCode, body: Value) -> Response<Body> {
    let mut r = proxy::full(status, body.to_string());
    r.headers_mut().insert("content-type", http::HeaderValue::from_static("application/json"));
    r
}

fn empty(status: StatusCode) -> Response<Body> {
    proxy::full(status, "")
}

/// The uid owning the client end of a loopback connection, from the
/// kernel's socket table: the socket whose local port is the peer's and
/// whose remote port is the listener's.
fn loopback_peer_uid(table: &str, peer_port: u16, listen_port: u16) -> Option<u32> {
    let port = |addr: &str| u16::from_str_radix(addr.rsplit_once(':')?.1, 16).ok();
    table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        (f.len() > 7 && port(f[1]) == Some(peer_port) && port(f[2]) == Some(listen_port))
            .then(|| f[7].parse().ok())
            .flatten()
    })
}

async fn peer_uid(peer: SocketAddr, listen_port: u16) -> Option<u32> {
    let file = if peer.is_ipv4() { "/proc/net/tcp" } else { "/proc/net/tcp6" };
    let table = tokio::fs::read_to_string(file).await.ok()?;
    loopback_peer_uid(&table, peer.port(), listen_port)
}

/// The request line and `Content-Length` of an HTTP/1.1 request head.
fn parse_head(head: &[u8]) -> Option<(String, String, usize)> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_owned(), first.next()?.to_owned());
    let length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(Some(0), |(_, v)| v.trim().parse().ok())?;
    Some((method, target, length))
}

impl Session {
    /// The container a Docker name means in this session, and whether the
    /// name is its sidecar's. workerd names a container
    /// `workerd-<session>-<uniqueKey>-<object id>`.
    fn resolve<'a>(&self, name: &'a str) -> Option<(&'a str, &ClassInfo, bool)> {
        let (base, sidecar) = match name.strip_suffix("-proxy") {
            Some(b) => (b, true),
            None => (name, false),
        };
        let rest = base.strip_prefix("workerd-")?.strip_prefix(self.id.as_str())?.strip_prefix('-')?;
        let class = self.classes.iter().find_map(|(key, class)| {
            let object = rest.strip_prefix(key.as_str())?.strip_prefix('-')?;
            (!object.is_empty() && object.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(class)
        })?;
        Some((base, class, sidecar))
    }
}

impl Shim {
    pub fn new(cfg: ContainersConfig, internal_token: String) -> Arc<Self> {
        Arc::new(Self {
            http: crate::tls::reqwest(reqwest::Client::builder(), &[crate::tls::Role::Containers])
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(70))
                .build()
                .expect("HTTP client"),
            exec_http: crate::tls::reqwest(reqwest::Client::builder(), &[crate::tls::Role::Containers])
                .connect_timeout(Duration::from_secs(2))
                .pool_max_idle_per_host(0)
                .build()
                .expect("HTTP client"),
            execs: Mutex::new(HashMap::new()),
            exec_pids: std::sync::atomic::AtomicU32::new(1000),
            // The sidecar closes a connection after each answer.
            sidecars: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()
                .expect("HTTP client"),
            cfg,
            internal_token,
            boxes: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &ContainersConfig {
        &self.cfg
    }

    /// Starts the egress relay and removes instances a previous run of this
    /// agent left behind.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(self.cfg.egress_listen)
            .await
            .with_context(|| format!("binding the container egress relay to {}", self.cfg.egress_listen))?;
        tracing::info!(addr = %self.cfg.egress_listen, "container egress relay listening");
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let me = me.clone();
                        tokio::spawn(async move { me.relay(stream, peer).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "egress relay accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            // The controller may start after the agent.
            for _ in 0..30 {
                match me.controller(Method::DELETE, "/internal/v1/containers", &[], None).await {
                    Ok((status, _)) if status.is_success() => return,
                    Ok((status, body)) => {
                        tracing::warn!(%status, %body, "resetting this runner's containers failed")
                    }
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "the containers controller is not reachable yet")
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Ok(())
    }

    /// One call to the controller, as this runner.
    async fn controller(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.cfg.controller_url))
            .bearer_auth(&self.internal_token)
            .query(&[("runner", self.cfg.runner.as_str())])
            .query(query);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        Ok((status, resp.json().await.unwrap_or(Value::Null)))
    }

    async fn instance(&self, id: &str, until: &str, timeout: Duration) -> Result<Instance> {
        let query = [("until", until.to_owned()), ("timeout_ms", timeout.as_millis().to_string())];
        let (status, body) =
            self.controller(Method::GET, &format!("/internal/v1/containers/{id}"), &query, None).await?;
        anyhow::ensure!(status.is_success(), "the containers controller answered {status}: {body}");
        Ok(serde_json::from_value(body)?)
    }

    async fn remove_instance(&self, id: &str, grace: u32, reason: &str) {
        let query = [("grace", grace.to_string()), ("reason", reason.to_owned())];
        match self.controller(Method::DELETE, &format!("/internal/v1/containers/{id}"), &query, None).await {
            Ok((status, _)) if status.is_success() => {}
            Ok((status, body)) => {
                tracing::warn!(instance = %id, %status, %body, "removing a container failed")
            }
            Err(e) => {
                tracing::warn!(instance = %id, error = %format!("{e:#}"), "removing a container failed")
            }
        }
    }

    // ---- Sessions ----

    /// Opens the Docker socket of an account process at `path`.
    pub fn serve(self: &Arc<Self>, session: Session, path: &Path) -> Result<()> {
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        if let Some(uid) = session.uid {
            // Only the account's own process may connect.
            super::uids::own(path, uid, 0o600)?;
        }
        let session = Arc::new(session);
        let id = session.id.clone();
        let me = self.clone();
        let handler = Arc::new(move |req: Request<Incoming>| {
            let (me, session) = (me.clone(), session.clone());
            async move { me.handle(&session, req).await }
        });
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => proxy::serve_connection(stream, handler.clone()),
                    Err(e) => {
                        tracing::error!(error = %e, "container socket accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
        self.sessions.lock().expect("poisoned").insert(id, (task, path.to_owned()));
        Ok(())
    }

    /// Ends an account process's session: closes its socket and removes its
    /// containers. Durable Objects that move to another process start their
    /// containers again there.
    pub async fn end_session(&self, session: &str) {
        let Some((task, path)) = self.sessions.lock().expect("poisoned").remove(session) else { return };
        task.abort();
        let _ = std::fs::remove_file(path);
        let ended: Vec<Arc<ContainerBox>> = {
            let mut boxes = self.boxes.lock().expect("poisoned");
            let names: Vec<String> =
                boxes.iter().filter(|(_, b)| b.session.id == session).map(|(n, _)| n.clone()).collect();
            names.iter().filter_map(|n| boxes.remove(n)).collect()
        };
        for b in &ended {
            if let Some(s) = b.inner.lock().expect("poisoned").sidecar.take() {
                s.listener.abort();
            }
        }
        if ended.is_empty() {
            return;
        }
        let query = [("session", session.to_owned())];
        match self.controller(Method::DELETE, "/internal/v1/containers", &query, None).await {
            Ok((status, _)) if status.is_success() => {}
            Ok((status, body)) => {
                tracing::warn!(session, %status, %body, "removing a session's containers failed")
            }
            Err(e) => {
                tracing::warn!(session, error = %format!("{e:#}"), "removing a session's containers failed")
            }
        }
    }

    fn find(&self, name: &str) -> Option<Arc<ContainerBox>> {
        self.boxes.lock().expect("poisoned").get(name).cloned()
    }

    // ---- The Docker API ----

    async fn handle(self: &Arc<Self>, session: &Arc<Session>, req: Request<Incoming>) -> Response<Body> {
        let began = Instant::now();
        let (method, target) = (req.method().clone(), req.uri().to_string());
        let resp = self.route(session, req).await;
        tracing::debug!(account = %session.account, %method, %target, status = resp.status().as_u16(), took = ?began.elapsed(), "docker API call");
        resp
    }

    async fn route(self: &Arc<Self>, session: &Arc<Session>, req: Request<Incoming>) -> Response<Body> {
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        let query: HashMap<String, String> =
            url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
                .into_owned()
                .collect();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let unsupported = |operation: &'static str| {
            crate::metrics::AGENT_CONTAINER_UNSUPPORTED.inc(&[operation]);
            docker_error(
                StatusCode::NOT_IMPLEMENTED,
                format!("Selvedge does not support this container operation ({operation})"),
            )
        };
        match (&method, segments.as_slice()) {
            // Snapshots are not supported, so there are no snapshot volumes.
            (&Method::GET, ["volumes"]) => {
                docker_json(StatusCode::OK, json!({ "Volumes": [], "Warnings": [] }))
            }
            // workerd opens its egress listener on the bridge's gateway:
            // loopback, where only the agent's relay reaches it.
            (&Method::GET, ["networks", "bridge"]) => docker_json(
                StatusCode::OK,
                json!({
                    "Name": "bridge",
                    "Id": "selvedge",
                    "IPAM": { "Config": [{ "Subnet": "127.0.0.0/8", "Gateway": "127.0.0.1" }] },
                }),
            ),
            (&Method::POST, ["containers", "create"]) => {
                let name = query.get("name").cloned().unwrap_or_default();
                let body = match http_body_util::Limited::new(req.into_body(), MAX_BODY).collect().await {
                    Ok(b) => b.to_bytes(),
                    Err(_) => return docker_error(StatusCode::BAD_REQUEST, "request body too large"),
                };
                let Ok(body) = serde_json::from_slice::<Value>(&body) else {
                    return docker_error(StatusCode::BAD_REQUEST, "invalid JSON");
                };
                self.create(session, &name, &body).await
            }
            (_, ["containers", name, rest @ ..]) => {
                let Some((base, class, sidecar)) = session.resolve(name) else {
                    return docker_error(StatusCode::NOT_FOUND, format!("No such container: {name}"));
                };
                match (&method, rest, sidecar) {
                    (&Method::GET, ["json"], true) => self.inspect_sidecar(base),
                    (&Method::GET, ["json"], false) => self.inspect_app(base, class),
                    (&Method::POST, ["start"], true) => match self.find(base) {
                        Some(b) if b.inner.lock().expect("poisoned").sidecar.is_some() => {
                            empty(StatusCode::NO_CONTENT)
                        }
                        _ => docker_error(StatusCode::NOT_FOUND, format!("No such container: {name}")),
                    },
                    (&Method::POST, ["start"], false) => self.start_app(base).await,
                    (&Method::DELETE, [], true) => self.remove_sidecar(base),
                    (&Method::DELETE, [], false) => self.remove_app(base, 0, "destroyed").await,
                    (&Method::POST, ["stop"], false) => match self.remove_app(base, 10, "destroyed").await {
                        r if r.status() == StatusCode::NOT_FOUND => empty(StatusCode::NOT_MODIFIED),
                        r => r,
                    },
                    (&Method::POST, ["kill"], false) => {
                        self.kill(base, query.get("signal").map_or("SIGKILL", String::as_str)).await
                    }
                    (&Method::POST, ["wait"], true) => {
                        docker_json(StatusCode::OK, json!({ "StatusCode": 0 }))
                    }
                    (&Method::POST, ["wait"], false) => {
                        self.wait(base, query.get("condition").is_some_and(|c| c == "removed")).await
                    }
                    // workerd writes the sidecar's CA into the container;
                    // the instance gets the real one when it starts.
                    (&Method::PUT, ["archive"], false) if query.get("path").is_some_and(|p| p == "/etc") => {
                        empty(StatusCode::OK)
                    }
                    (_, ["archive"], _) => unsupported("archive"),
                    (&Method::POST, ["exec"], false) => {
                        let body = match http_body_util::Limited::new(req.into_body(), MAX_BODY)
                            .collect()
                            .await
                        {
                            Ok(b) => b.to_bytes(),
                            Err(_) => return docker_error(StatusCode::BAD_REQUEST, "request body too large"),
                        };
                        let Ok(body) = serde_json::from_slice::<Value>(&body) else {
                            return docker_error(StatusCode::BAD_REQUEST, "invalid JSON");
                        };
                        self.create_exec(session, base, &body)
                    }
                    (_, ["exec"], _) => unsupported("exec"),
                    _ => unsupported("container"),
                }
            }
            (_, ["exec", id, rest @ ..]) => {
                let Some(e) = self.execs.lock().expect("poisoned").get(*id).cloned() else {
                    return docker_error(StatusCode::NOT_FOUND, format!("No such exec instance: {id}"));
                };
                if e.session != session.id {
                    return docker_error(StatusCode::NOT_FOUND, format!("No such exec instance: {id}"));
                }
                match (&method, rest) {
                    (&Method::POST, ["start"]) => self.start_exec(id, e, req).await,
                    (&Method::GET, ["json"]) => {
                        let st = e.state.lock().expect("poisoned");
                        docker_json(
                            StatusCode::OK,
                            json!({
                                "ID": id,
                                "ContainerID": e.container,
                                "Running": st.running,
                                "ExitCode": st.exit_code,
                                "Pid": st.pid,
                            }),
                        )
                    }
                    (&Method::POST, ["resize"]) => {
                        let size = |k: &str| query.get(k).and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
                        let input = e.state.lock().expect("poisoned").input.clone();
                        if let Some(tx) = input {
                            let _ = tx
                                .send(crate::containers::exec::Input::Resize {
                                    cols: size("w"),
                                    rows: size("h"),
                                })
                                .await;
                        }
                        empty(StatusCode::OK)
                    }
                    _ => unsupported("exec"),
                }
            }
            (_, ["commit"]) | (_, ["volumes", ..]) => unsupported("snapshot"),
            (_, ["images", ..]) => unsupported("image"),
            _ => unsupported("other"),
        }
    }

    // ---- exec ----

    /// The instance a container's application runs as, if it runs.
    fn running_instance(&self, container: &str) -> Option<String> {
        let b = self.find(container)?;
        let inner = b.inner.lock().expect("poisoned");
        let app = inner.app.as_ref()?;
        (app.state == AppState::Running).then(|| app.instance.clone()).flatten()
    }

    fn create_exec(&self, session: &Arc<Session>, container: &str, body: &Value) -> Response<Body> {
        use crate::containers::exec::ExecSpec;
        if self.running_instance(container).is_none() {
            return docker_error(StatusCode::CONFLICT, format!("Container {container} is not running"));
        }
        let strings = |k: &str| -> Vec<String> {
            body.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default()
        };
        let flag = |k: &str| body.get(k).and_then(Value::as_bool).unwrap_or(false);
        let text = |k: &str| body.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        let spec = ExecSpec {
            cmd: strings("Cmd"),
            env: strings("Env"),
            working_dir: text("WorkingDir"),
            user: text("User"),
            tty: flag("Tty"),
            stdin: flag("AttachStdin"),
            stdout: flag("AttachStdout"),
            stderr: flag("AttachStderr"),
            cols: 0,
            rows: 0,
        };
        if spec.cmd.is_empty() {
            return docker_error(StatusCode::BAD_REQUEST, "No exec command specified");
        }
        let id = hex::encode(rand::random::<[u8; 32]>());
        let mut execs = self.execs.lock().expect("poisoned");
        execs.retain(|_, e| {
            e.state.lock().expect("poisoned").finished.is_none_or(|f| f.elapsed() < EXEC_KEEP)
        });
        execs.insert(
            id.clone(),
            Arc::new(Exec {
                session: session.id.clone(),
                container: container.to_owned(),
                spec,
                state: Mutex::new(ExecState::default()),
            }),
        );
        docker_json(StatusCode::CREATED, json!({ "Id": id }))
    }

    async fn start_exec(
        self: &Arc<Self>,
        id: &str,
        e: Arc<Exec>,
        mut req: Request<Incoming>,
    ) -> Response<Body> {
        use crate::containers::exec;
        use base64::Engine;
        let downstream = hyper::upgrade::on(&mut req);
        let body = match http_body_util::Limited::new(req.into_body(), MAX_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return docker_error(StatusCode::BAD_REQUEST, "request body too large"),
        };
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let detach = body.get("Detach").and_then(Value::as_bool).unwrap_or(false);
        let mut spec = e.spec.clone();
        if let Some(size) = body.get("ConsoleSize").and_then(Value::as_array) {
            let n = |i: usize| size.get(i).and_then(Value::as_u64).unwrap_or(0).min(u16::MAX.into()) as u16;
            (spec.rows, spec.cols) = (n(0), n(1));
        }
        if std::mem::replace(&mut e.state.lock().expect("poisoned").started, true) {
            return docker_error(StatusCode::CONFLICT, format!("Exec {id} has already started"));
        }
        let Some(instance) = self.running_instance(&e.container) else {
            return docker_error(StatusCode::CONFLICT, format!("Container {} is not running", e.container));
        };
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&spec).unwrap_or_default());
        let resp = self
            .exec_http
            .post(format!("{}/internal/v1/containers/{instance}/exec", self.cfg.controller_url))
            .bearer_auth(&self.internal_token)
            .query(&[("runner", self.cfg.runner.as_str())])
            .header(http::header::CONNECTION, "upgrade")
            .header(http::header::UPGRADE, exec::PROTOCOL)
            .header(exec::SPEC_HEADER, encoded)
            .send()
            .await;
        let upstream = match resp {
            Ok(r) if r.status() == StatusCode::SWITCHING_PROTOCOLS => match r.upgrade().await {
                Ok(u) => u,
                Err(err) => return docker_error(StatusCode::BAD_GATEWAY, format!("exec: {err}")),
            },
            Ok(r) => {
                let status = r.status();
                let message =
                    r.json::<Value>().await.ok().and_then(|v| v["error"].as_str().map(str::to_owned));
                return docker_error(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "exec: the containers controller answered {status}: {}",
                        message.unwrap_or_default()
                    ),
                );
            }
            Err(err) => return docker_error(StatusCode::BAD_GATEWAY, format!("exec: {err}")),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        {
            let mut st = e.state.lock().expect("poisoned");
            st.running = true;
            st.pid = self.exec_pids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            st.input = Some(tx.clone());
        }
        let tty = spec.tty;
        if detach {
            let _ = tx.send(exec::Input::CloseStdin).await;
            tokio::spawn(async move {
                bridge_exec(e, upstream, None::<tokio::io::DuplexStream>, tx, rx, tty).await
            });
            return docker_json(StatusCode::OK, json!({}));
        }
        tokio::spawn(async move {
            let down = match downstream.await {
                Ok(d) => Some(hyper_util::rt::TokioIo::new(d)),
                Err(err) => {
                    tracing::debug!(error = %err, "workerd's exec connection was not upgraded");
                    None
                }
            };
            bridge_exec(e, upstream, down, tx, rx, tty).await;
        });
        let content_type = if tty {
            "application/vnd.docker.raw-stream"
        } else {
            "application/vnd.docker.multiplexed-stream"
        };
        let mut resp = empty(StatusCode::SWITCHING_PROTOCOLS);
        let h = resp.headers_mut();
        h.insert(http::header::CONNECTION, http::HeaderValue::from_static("Upgrade"));
        h.insert(http::header::UPGRADE, http::HeaderValue::from_static("tcp"));
        h.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static(content_type));
        resp
    }

    async fn create(self: &Arc<Self>, session: &Arc<Session>, name: &str, body: &Value) -> Response<Body> {
        let Some((base, class, sidecar)) = session.resolve(name) else {
            return docker_error(StatusCode::BAD_REQUEST, format!("invalid container name: {name}"));
        };
        let b = {
            let mut boxes = self.boxes.lock().expect("poisoned");
            boxes
                .entry(base.to_owned())
                .or_insert_with(|| {
                    Arc::new(ContainerBox {
                        session: session.clone(),
                        class: class.clone(),
                        name: base.to_owned(),
                        inner: Mutex::new(BoxInner::default()),
                        op: tokio::sync::Mutex::new(()),
                    })
                })
                .clone()
        };
        let _op = b.op.lock().await;
        if sidecar {
            if b.inner.lock().expect("poisoned").sidecar.is_some() {
                return docker_error(StatusCode::CONFLICT, "the sidecar already exists");
            }
            // The port workerd will reach this container through.
            let listener = match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await {
                Ok(l) => l,
                Err(e) => {
                    return docker_error(StatusCode::INTERNAL_SERVER_ERROR, format!("opening a port: {e}"));
                }
            };
            let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
            let (me, bx) = (self.clone(), b.clone());
            let task = tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, peer)) => {
                            let (me, bx) = (me.clone(), bx.clone());
                            tokio::spawn(async move { me.ingress(&bx, stream, peer, port).await });
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                    }
                }
            });
            b.inner.lock().expect("poisoned").sidecar = Some(Sidecar { port, listener: task });
            return docker_json(StatusCode::CREATED, json!({ "Id": name, "Warnings": [] }));
        }
        // Only the application's own image: the controller starts nothing
        // else, whatever a Worker passes to `start()`.
        let image = body.get("Image").and_then(Value::as_str).unwrap_or_default();
        if image != class.image {
            return docker_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "start() asked for image `{image}`, but Selvedge runs only the image of the class's \
                     container application (`{}`)",
                    class.image
                ),
            );
        }
        let strings = |key: &str| -> Option<Vec<String>> {
            Some(body.get(key)?.as_array()?.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        };
        let mut inner = b.inner.lock().expect("poisoned");
        if inner.app.is_some() {
            return docker_error(StatusCode::CONFLICT, "the container already exists");
        }
        inner.app = Some(App {
            env: strings("Env").unwrap_or_default(),
            command: strings("Cmd").filter(|c| !c.is_empty()),
            instance: None,
            state: AppState::Created,
        });
        docker_json(StatusCode::CREATED, json!({ "Id": name, "Warnings": [] }))
    }

    fn inspect_sidecar(&self, base: &str) -> Response<Body> {
        let port =
            self.find(base).and_then(|b| b.inner.lock().expect("poisoned").sidecar.as_ref().map(|s| s.port));
        let Some(port) = port else {
            return docker_error(StatusCode::NOT_FOUND, "No such container");
        };
        docker_json(
            StatusCode::OK,
            json!({
                "Id": format!("{base}-proxy"),
                "State": { "Status": "running", "Running": true },
                "Config": { "Image": self.cfg.interceptor_image },
                "NetworkSettings": {
                    "Ports": { format!("{INGRESS_PORT}/tcp"): [{ "HostIp": "127.0.0.1", "HostPort": port.to_string() }] },
                },
            }),
        )
    }

    fn inspect_app(&self, base: &str, class: &ClassInfo) -> Response<Body> {
        let state = self
            .find(base)
            .and_then(|b| b.inner.lock().expect("poisoned").app.as_ref().map(|a| a.state.clone()));
        let Some(state) = state else {
            return docker_error(StatusCode::NOT_FOUND, "No such container");
        };
        let (status, running, code) = match state {
            AppState::Created => ("created", false, 0),
            AppState::Running => ("running", true, 0),
            AppState::Exited(code) => ("exited", false, code),
        };
        docker_json(
            StatusCode::OK,
            json!({
                "Id": base,
                "State": { "Status": status, "Running": running, "ExitCode": code },
                "Config": { "Image": class.image },
            }),
        )
    }

    /// `POST /containers/<app>/start`: asks the controller for an instance
    /// and answers once its application runs.
    async fn start_app(self: &Arc<Self>, base: &str) -> Response<Body> {
        let Some(b) = self.find(base) else {
            return docker_error(StatusCode::NOT_FOUND, "No such container");
        };
        let _op = b.op.lock().await;
        let request = {
            let inner = b.inner.lock().expect("poisoned");
            let Some(app) = &inner.app else {
                return docker_error(StatusCode::NOT_FOUND, "No such container");
            };
            if app.state != AppState::Created {
                return empty(StatusCode::NOT_MODIFIED);
            }
            StartRequest {
                account: b.session.account.clone(),
                script: b.class.script.clone(),
                class: b.class.class.clone(),
                name: b.name.clone(),
                session: b.session.id.clone(),
                env: app.env.clone(),
                command: app.command.clone(),
                egress_port: self.cfg.egress_listen.port(),
                egress: inner
                    .egress
                    .clone()
                    .unwrap_or_else(|| json!({ "port": self.cfg.egress_listen.port() })),
                runner: self.cfg.runner.clone(),
            }
        };
        let began = Instant::now();
        let fail = |status: StatusCode, message: String| {
            crate::metrics::AGENT_CONTAINER_START_DURATION.observe(&["error"], began.elapsed());
            tracing::warn!(account = %b.session.account, class = %b.class.class, %message, "a container did not start");
            docker_error(status, message)
        };
        let body = serde_json::to_value(&request).expect("serializable");
        let id = match self.controller(Method::POST, "/internal/v1/containers", &[], Some(body)).await {
            Ok((status, v)) if status.is_success() => v["id"].as_str().unwrap_or_default().to_owned(),
            Ok((StatusCode::NOT_FOUND, _)) => {
                return fail(
                    StatusCode::NOT_FOUND,
                    "No such image: the Durable Object class has no container application".into(),
                );
            }
            Ok((_, v)) => {
                let m = v["error"].as_str().unwrap_or("the containers controller refused the request");
                return fail(StatusCode::INTERNAL_SERVER_ERROR, m.to_owned());
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "the containers controller is unreachable");
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "containers are unavailable right now".into(),
                );
            }
        };
        if let Some(app) = b.inner.lock().expect("poisoned").app.as_mut() {
            app.instance = Some(id.clone());
        }
        let mut last = None;
        let outcome = loop {
            let left = self.cfg.start_timeout.saturating_sub(began.elapsed());
            if left.is_zero() {
                break Err(format!(
                    "the container did not start within {} s{}",
                    self.cfg.start_timeout.as_secs(),
                    last.map(|m: String| format!(" ({m})")).unwrap_or_default()
                ));
            }
            match self.instance(&id, "started", left.min(Duration::from_secs(30))).await {
                Ok(i) => match i.phase {
                    Phase::Pending => last = i.message,
                    Phase::Running => break Ok((AppState::Running, i.ip)),
                    // Started and already done: `monitor()` reports the code.
                    Phase::Exited => break Ok((AppState::Exited(i.exit_code.unwrap_or(1)), None)),
                    Phase::Failed => {
                        break Err(format!(
                            "the container could not start: {}",
                            i.message.unwrap_or_else(|| "unknown reason".into())
                        ));
                    }
                    Phase::Gone => break Err("the container was removed while it started".into()),
                },
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "asking for a container's state failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        match outcome {
            Ok((state, ip)) => {
                let mut inner = b.inner.lock().expect("poisoned");
                inner.ip = ip;
                if let Some(app) = inner.app.as_mut() {
                    app.state = state;
                }
                crate::metrics::AGENT_CONTAINER_START_DURATION.observe(&["ok"], began.elapsed());
                empty(StatusCode::NO_CONTENT)
            }
            Err(message) => {
                self.remove_instance(&id, 0, "failed").await;
                b.inner.lock().expect("poisoned").app = None;
                fail(StatusCode::INTERNAL_SERVER_ERROR, message)
            }
        }
    }

    /// Removes the application's instance and waits until it is gone.
    async fn remove_app(&self, base: &str, grace: u32, reason: &str) -> Response<Body> {
        let Some(b) = self.find(base) else {
            return docker_error(StatusCode::NOT_FOUND, "No such container");
        };
        let _op = b.op.lock().await;
        let instance = {
            let mut inner = b.inner.lock().expect("poisoned");
            let Some(app) = inner.app.take() else {
                return docker_error(StatusCode::NOT_FOUND, "No such container");
            };
            inner.ip = None;
            app.instance
        };
        if let Some(id) = instance {
            self.remove_instance(&id, grace, reason).await;
            let deadline = Instant::now() + Duration::from_secs(u64::from(grace) + 30);
            while Instant::now() < deadline {
                match self.instance(&id, "removed", Duration::from_secs(10)).await {
                    Ok(i) if i.phase == Phase::Gone => break,
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
        }
        empty(StatusCode::NO_CONTENT)
    }

    fn remove_sidecar(&self, base: &str) -> Response<Body> {
        let sidecar = self.find(base).and_then(|b| b.inner.lock().expect("poisoned").sidecar.take());
        match sidecar {
            Some(s) => {
                s.listener.abort();
                empty(StatusCode::NO_CONTENT)
            }
            None => docker_error(StatusCode::NOT_FOUND, "No such container"),
        }
    }

    /// `signal()`: an instance can be stopped, at once or with time to
    /// finish, but no other signal can be delivered into a pod.
    async fn kill(&self, base: &str, signal: &str) -> Response<Body> {
        let running = self.find(base).is_some_and(|b| {
            b.inner.lock().expect("poisoned").app.as_ref().is_some_and(|a| a.state == AppState::Running)
        });
        if !running {
            return docker_error(StatusCode::CONFLICT, "the container is not running");
        }
        let grace = match signal.trim_start_matches("SIG") {
            "KILL" | "9" => 0,
            "TERM" | "15" | "INT" | "2" | "QUIT" | "3" => 10,
            _ => {
                crate::metrics::AGENT_CONTAINER_UNSUPPORTED.inc(&["signal"]);
                return docker_error(
                    StatusCode::NOT_IMPLEMENTED,
                    format!(
                        "Selvedge delivers only SIGTERM, SIGINT, SIGQUIT and SIGKILL to a container, not {signal}"
                    ),
                );
            }
        };
        // The instance stops with its application; `monitor()` reports it.
        let instance = self
            .find(base)
            .and_then(|b| b.inner.lock().expect("poisoned").app.as_ref().and_then(|a| a.instance.clone()));
        if let Some(id) = instance {
            self.remove_instance(&id, grace, "signal").await;
        }
        empty(StatusCode::NO_CONTENT)
    }

    /// `POST /containers/<app>/wait`: `monitor()` waits for the application
    /// to exit; `condition=removed` waits for a removal, which
    /// [`Shim::remove_app`] already did.
    async fn wait(&self, base: &str, removed: bool) -> Response<Body> {
        let Some(b) = self.find(base) else {
            return docker_error(StatusCode::NOT_FOUND, "No such container");
        };
        let app =
            b.inner.lock().expect("poisoned").app.as_ref().map(|a| (a.instance.clone(), a.state.clone()));
        let (instance, state) = match app {
            None if removed => return docker_json(StatusCode::OK, json!({ "StatusCode": 0 })),
            None => return docker_error(StatusCode::NOT_FOUND, "No such container"),
            Some(a) => a,
        };
        if let AppState::Exited(code) = state {
            return docker_json(StatusCode::OK, json!({ "StatusCode": code }));
        }
        let Some(id) = instance else {
            return docker_json(StatusCode::OK, json!({ "StatusCode": 0 }));
        };
        let code = loop {
            match self.instance(&id, "exited", Duration::from_secs(30)).await {
                Ok(i) => match i.phase {
                    Phase::Exited => break i.exit_code.unwrap_or(1),
                    Phase::Failed => break 1,
                    Phase::Gone => break KILLED,
                    Phase::Pending | Phase::Running => {}
                },
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "waiting for a container failed");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            // Removed through this socket meanwhile (`destroy()`).
            let current = b.inner.lock().expect("poisoned").app.as_ref().and_then(|a| a.instance.clone());
            if current.as_deref() != Some(id.as_str()) {
                break KILLED;
            }
        };
        // The application is done: the pod has nothing left to do.
        let ours = {
            let mut inner = b.inner.lock().expect("poisoned");
            match inner.app.as_mut() {
                Some(app) if app.instance.as_deref() == Some(id.as_str()) => {
                    app.state = AppState::Exited(code);
                    inner.ip = None;
                    true
                }
                _ => false,
            }
        };
        if ours {
            self.remove_instance(&id, 0, "exited").await;
        }
        docker_json(StatusCode::OK, json!({ "StatusCode": code }))
    }

    // ---- Traffic ----

    /// A connection workerd opened to a container's ingress port: a tunnel
    /// into the container (`CONNECT`), or a control call to its sidecar.
    async fn ingress(
        &self,
        b: &Arc<ContainerBox>,
        mut stream: tokio::net::TcpStream,
        peer: SocketAddr,
        port: u16,
    ) {
        let _ = stream.set_nodelay(true);
        if let Some(uid) = b.session.uid {
            // Every account's process can dial loopback; only this
            // account's may use this container's port.
            if peer_uid(peer, port).await != Some(uid) {
                tracing::warn!(account = %b.session.account, "refused a container connection from another user");
                return;
            }
        }
        let mut buf = Vec::with_capacity(1024);
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if buf.len() > 16 << 10 {
                return;
            }
            match tokio::time::timeout(Duration::from_secs(30), stream.read_buf(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => return,
            }
        };
        let Some((method, target, length)) = parse_head(&buf[..head_end]) else { return };
        let ip = b.inner.lock().expect("poisoned").ip;
        if method == "CONNECT" {
            let Some(ip) = ip else {
                let _ =
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n").await;
                return;
            };
            let upstream = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::TcpStream::connect(SocketAddr::new(ip, INGRESS_PORT)),
            )
            .await;
            let Ok(Ok(mut upstream)) = upstream else {
                let _ = stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n").await;
                return;
            };
            let _ = upstream.set_nodelay(true);
            if upstream.write_all(&buf).await.is_ok() {
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
            }
            return;
        }
        if length > MAX_BODY {
            return;
        }
        while buf.len() < head_end + length {
            match tokio::time::timeout(Duration::from_secs(30), stream.read_buf(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => return,
            }
        }
        let body = &buf[head_end..head_end + length];
        let (status, answer) = self.sidecar_call(b, ip, &method, &target, body).await;
        let head = format!(
            "HTTP/1.1 {} {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            answer.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(&answer).await;
        let _ = stream.shutdown().await;
    }

    /// A control call to a container's sidecar. Without an instance, the
    /// stand-in answers; with one, the real sidecar does.
    async fn sidecar_call(
        &self,
        b: &ContainerBox,
        ip: Option<IpAddr>,
        method: &str,
        target: &str,
        body: &[u8],
    ) -> (StatusCode, Vec<u8>) {
        let url = ip.map(|ip| format!("http://{}{target}", SocketAddr::new(ip, INGRESS_PORT)));
        match (method, target) {
            ("PUT", "/egress") => {
                let Ok(mut config) = serde_json::from_slice::<Value>(body) else {
                    return (StatusCode::BAD_REQUEST, b"invalid JSON".to_vec());
                };
                // The port is workerd's listener on loopback. The sidecar
                // gets the agent's relay instead, which forwards there.
                let workerd_port =
                    config.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok());
                config["port"] = json!(self.cfg.egress_listen.port());
                {
                    let mut inner = b.inner.lock().expect("poisoned");
                    inner.workerd_egress = workerd_port.or(inner.workerd_egress);
                    inner.egress = Some(config.clone());
                }
                match url {
                    Some(url) => match self.sidecars.put(url).json(&config).send().await {
                        Ok(r) => {
                            let status =
                                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                            (status, r.bytes().await.map(|b| b.to_vec()).unwrap_or_default())
                        }
                        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string().into_bytes()),
                    },
                    None => (StatusCode::OK, Vec::new()),
                }
            }
            ("GET", "/ca") => match url {
                Some(url) => match self.sidecars.get(url).send().await {
                    Ok(r) if r.status().is_success() => {
                        (StatusCode::OK, r.bytes().await.map(|b| b.to_vec()).unwrap_or_default())
                    }
                    _ => (StatusCode::OK, PLACEHOLDER_CA.as_bytes().to_vec()),
                },
                None => (StatusCode::OK, PLACEHOLDER_CA.as_bytes().to_vec()),
            },
            _ => (StatusCode::NOT_FOUND, Vec::new()),
        }
    }

    /// A connection from an instance's sidecar: a container's outbound
    /// traffic, on its way to the egress listener workerd opened for it.
    async fn relay(&self, mut stream: tokio::net::TcpStream, peer: SocketAddr) {
        let source = peer.ip().to_canonical();
        let port = self.boxes.lock().expect("poisoned").values().find_map(|b| {
            let inner = b.inner.lock().expect("poisoned");
            (inner.ip.map(|ip| ip.to_canonical()) == Some(source)).then_some(inner.workerd_egress).flatten()
        });
        let Some(port) = port else {
            crate::metrics::AGENT_CONTAINER_EGRESS_REFUSED.inc(&[]);
            tracing::debug!(%peer, "refused an egress connection from an address that is not a container's");
            return;
        };
        let _ = stream.set_nodelay(true);
        match tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).await {
            Ok(mut workerd) => {
                let _ = workerd.set_nodelay(true);
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut workerd).await;
            }
            Err(e) => tracing::debug!(%peer, error = %e, "workerd's egress listener is not reachable"),
        }
    }
}

/// Carries a started exec between workerd's hijacked connection
/// (`downstream`, absent when detached) and the controller's (`upstream`),
/// and records how it ended.
async fn bridge_exec<U, D>(
    e: Arc<Exec>,
    upstream: U,
    downstream: Option<D>,
    tx: tokio::sync::mpsc::Sender<crate::containers::exec::Input>,
    mut rx: tokio::sync::mpsc::Receiver<crate::containers::exec::Input>,
    tty: bool,
) where
    U: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    D: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    use crate::containers::exec::{self, Input};
    let (mut ur, mut uw) = tokio::io::split(upstream);
    let writer = tokio::spawn(async move {
        while let Some(i) = rx.recv().await {
            let r = match i {
                Input::Stdin(b) => exec::write_frame(&mut uw, exec::STDIN, &b).await,
                Input::CloseStdin => exec::write_frame(&mut uw, exec::CLOSE_STDIN, &[]).await,
                Input::Resize { cols, rows } => {
                    let size = json!({ "cols": cols, "rows": rows }).to_string();
                    exec::write_frame(&mut uw, exec::RESIZE, size.as_bytes()).await
                }
            };
            if r.is_err() {
                return;
            }
        }
    });
    let (mut dw, stdin) = match downstream {
        Some(d) => {
            let (mut dr, dw) = tokio::io::split(d);
            let stdin_tx = tx.clone();
            let stdin = tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match dr.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            let _ = stdin_tx.send(Input::CloseStdin).await;
                            return;
                        }
                        Ok(n) => {
                            if stdin_tx
                                .send(Input::Stdin(bytes::Bytes::copy_from_slice(&buf[..n])))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            });
            (Some(dw), Some(stdin))
        }
        None => (None, None),
    };
    drop(tx);
    let mut exit = None;
    loop {
        match exec::read_frame(&mut ur).await {
            // workerd writes every frame into the output stream, which the
            // Worker may never read: an empty one would block it for good.
            Ok(Some((exec::STDOUT | exec::STDERR, payload))) if payload.is_empty() => {}
            Ok(Some((kind @ (exec::STDOUT | exec::STDERR), payload))) => {
                if let Some(w) = dw.as_mut() {
                    let r = if tty {
                        w.write_all(&payload).await
                    } else {
                        exec::write_frame(w, kind, &payload).await
                    };
                    if r.is_err() {
                        dw = None;
                    }
                }
            }
            Ok(Some((exec::EXIT, payload))) => {
                exit = serde_json::from_slice::<exec::Exit>(&payload).ok();
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(container = %e.container, error = %format!("{err:#}"), "reading an exec's output failed");
                break;
            }
        }
    }
    tracing::debug!(container = %e.container, ?exit, attached = dw.is_some(), "an exec's output ended");
    let code = match exit {
        Some(exec::Exit { exit_code: Some(code), .. }) => code,
        other => {
            let message =
                other.and_then(|x| x.error).unwrap_or_else(|| "the exec connection ended".to_owned());
            tracing::warn!(container = %e.container, %message, "an exec failed");
            // Say why on stderr, where the caller looks.
            if let Some(w) = dw.as_mut() {
                let line = format!("selvedge: {message}\n");
                let _ = if tty {
                    w.write_all(line.as_bytes()).await
                } else {
                    exec::write_frame(w, exec::STDERR, line.as_bytes()).await
                };
            }
            EXEC_FAILED
        }
    };
    {
        let mut st = e.state.lock().expect("poisoned");
        st.running = false;
        st.exit_code = Some(code);
        st.finished = Some(Instant::now());
        st.input = None;
    }
    if let Some(mut w) = dw {
        let r = w.shutdown().await;
        tracing::debug!(container = %e.container, result = ?r, "closed an exec's stream to workerd");
    }
    if let Some(s) = stdin {
        s.abort();
    }
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            account: "0123456789abcdef0123456789abcdef".parse().unwrap(),
            id: "ab12cd34-0123456789ab-g7-1".into(),
            uid: None,
            classes: [(
                "do-0123456789012345678901234567890123456789".to_owned(),
                ClassInfo { script: "s".into(), class: "Box".into(), image: "r/a/app:1".into() },
            )]
            .into(),
        }
    }

    #[test]
    fn names() {
        let s = session();
        let app = "workerd-ab12cd34-0123456789ab-g7-1-do-0123456789012345678901234567890123456789-aabbcc";
        let (base, class, sidecar) = s.resolve(app).unwrap();
        assert_eq!((base, class.class.as_str(), sidecar), (app, "Box", false));
        let proxy = format!("{app}-proxy");
        let (base, _, sidecar) = s.resolve(&proxy).unwrap();
        assert_eq!((base, sidecar), (app, true));
        // Another session's container, an unknown class, a path.
        assert!(s.resolve(&app.replace("g7-1", "g8-1")).is_none());
        assert!(s.resolve(&app.replace("do-0", "do-1")).is_none());
        assert!(
            s.resolve("workerd-ab12cd34-0123456789ab-g7-1-do-0123456789012345678901234567890123456789-a/b")
                .is_none()
        );
        assert!(s.resolve("anything").is_none());
    }

    #[test]
    fn socket_table() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:9C40 0100007F:1F90 01 00000000:00000000 00:00000000 00000000 10007        0 1234 1 0 20 4 30 10 -1
   1: 0100007F:1F90 0100007F:9C40 01 00000000:00000000 00:00000000 00000000     0        0 1235 1 0 20 4 30 10 -1
";
        assert_eq!(loopback_peer_uid(table, 40000, 8080), Some(10007));
        assert_eq!(loopback_peer_uid(table, 8080, 40000), Some(0));
        assert_eq!(loopback_peer_uid(table, 40001, 8080), None);
    }

    #[test]
    fn heads() {
        let h = b"PUT /egress HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\n\r\n";
        assert_eq!(parse_head(h), Some(("PUT".into(), "/egress".into(), 12)));
        let h = b"CONNECT 127.0.0.1:8080 HTTP/1.1\r\nX-Dst-Addr: 127.0.0.1:8080\r\n\r\n";
        assert_eq!(parse_head(h), Some(("CONNECT".into(), "127.0.0.1:8080".into(), 0)));
        assert_eq!(parse_head(b"PUT /egress HTTP/1.1\r\ncontent-length: x\r\n\r\n"), None);
    }
}
