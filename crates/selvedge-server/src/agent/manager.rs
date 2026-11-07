//! One workerd process per account, started on demand, replaced on deploy and
//! stopped when idle.
//!
//! A deploy starts the new process before stopping the old one, so requests keep
//! flowing. In cluster mode the old process drains on SIGTERM and deregisters,
//! so the new one takes its Durable Objects on their next request. Without
//! cluster mode (single-node Durable Objects) the old process must stop first,
//! because two processes cannot open the same local SQLite files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use selvedge_core::{AccountBundle, AccountId};
use selvedge_workerd::{ClusterOptions, RenderOptions, WorkerdProcess, render};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::AgentConfig;

/// How long a known generation is trusted before a request triggers a check.
/// Deploys normally arrive sooner, through [`Manager::watch`].
const GENERATION_TTL: Duration = Duration::from_secs(30);
/// How often the agent asks which accounts changed.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_GRACE: Duration = Duration::from_secs(30);
/// How long a process over its limits may take to drain before SIGKILL.
const LIMIT_GRACE: Duration = Duration::from_secs(3);

/// A running account process.
pub struct Live {
    pub generation: i64,
    pub sockets: HashMap<String, PathBuf>,
    /// Gradual deployments: script to (canary share 0-100, canary version id).
    pub splits: HashMap<String, (f64, String)>,
    process: Mutex<Option<WorkerdProcess>>,
    render_dir: PathBuf,
    socket_dir: PathBuf,
    do_dir: Option<PathBuf>,
}

impl Live {
    async fn alive(&self) -> bool {
        self.process.lock().await.as_mut().is_some_and(WorkerdProcess::alive)
    }

    async fn stop(&self, grace: Duration) {
        if let Some(p) = self.process.lock().await.take() {
            p.stop(grace).await;
        }
        let _ = std::fs::remove_dir_all(&self.render_dir);
        let _ = std::fs::remove_dir_all(&self.socket_dir);
        if let Some(d) = &self.do_dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

#[derive(Default)]
struct Slot {
    live: Option<Arc<Live>>,
    checked_at: Option<Instant>,
    binding_socket: Option<PathBuf>,
    /// Bumped on each start so render directories never collide.
    starts: u64,
    /// A background generation check is running.
    refreshing: bool,
}

/// Per-account state. `slot` is only ever held briefly; `start` serializes
/// generation checks and process starts, which can take a while.
#[derive(Default)]
struct AccountState {
    slot: Mutex<Slot>,
    start: Mutex<()>,
}

pub struct Manager {
    cfg: Arc<AgentConfig>,
    /// For bundles and blobs.
    http: reqwest::Client,
    /// For generation checks: short timeouts, since a slow API must not hold up
    /// a refresh for long.
    control: reqwest::Client,
    max_date: String,
    accounts: StdMutex<HashMap<AccountId, Arc<AccountState>>>,
    last_used: StdMutex<HashMap<AccountId, Instant>>,
    /// Scripts someone is tailing, from the change feed.
    tails: super::bindings::Tailed,
    uids: Option<super::uids::Uids>,
    /// The agent's S3 proxy for Durable Object storage.
    s3_proxy: Option<std::net::SocketAddr>,
    /// Where account cgroups go, when the cgroup tree is writable.
    cgroup_dir: Option<PathBuf>,
}

pub enum Lookup {
    Ready(Arc<Live>),
    /// The account does not exist or has no scripts.
    NotFound,
}

impl Manager {
    pub fn new(
        cfg: Arc<AgentConfig>,
        max_date: String,
        s3_proxy: Option<std::net::SocketAddr>,
        cgroup_dir: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        let uids = cfg
            .account_uids
            .clone()
            .map(|r| super::uids::Uids::open(r, cfg.state_dir.join("uids.json")))
            .transpose()?;
        Ok(Arc::new(Self {
            uids,
            s3_proxy,
            cgroup_dir,
            cfg,
            http: crate::tls::reqwest(reqwest::Client::builder(), &[crate::tls::Role::Api])
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("HTTP client"),
            control: crate::tls::reqwest(reqwest::Client::builder(), &[crate::tls::Role::Api])
                .connect_timeout(Duration::from_secs(1))
                .timeout(Duration::from_secs(3))
                .build()
                .expect("HTTP client"),
            max_date,
            accounts: StdMutex::new(HashMap::new()),
            last_used: StdMutex::new(HashMap::new()),
            tails: super::bindings::Tailed::default(),
        }))
    }

    fn account(&self, account: &AccountId) -> Arc<AccountState> {
        self.accounts.lock().expect("poisoned").entry(account.clone()).or_default().clone()
    }

    /// The account's process. A running process is returned at once; checking
    /// for a new generation happens in the background, so the request path
    /// never waits on the control plane.
    pub async fn ensure(self: &Arc<Self>, account: &AccountId) -> Result<Lookup> {
        self.last_used.lock().expect("poisoned").insert(account.clone(), Instant::now());
        let state = self.account(account);
        {
            let mut slot = state.slot.lock().await;
            if let Some(live) = slot.live.clone() {
                if live.alive().await {
                    let stale = slot.checked_at.is_none_or(|t| t.elapsed() >= GENERATION_TTL);
                    if stale && !slot.refreshing {
                        slot.refreshing = true;
                        let (me, account) = (self.clone(), account.clone());
                        tokio::spawn(async move {
                            if let Err(e) = me.rebuild(&account).await {
                                tracing::warn!(%account, error = %format!("{e:#}"), "refreshing the account failed");
                            }
                            me.account(&account).slot.lock().await.refreshing = false;
                        });
                    }
                    return Ok(Lookup::Ready(live));
                }
            }
        }
        // No running process: this request has to wait for one.
        self.rebuild(account).await
    }

    /// Follows account changes and refreshes the accounts running here, so a
    /// deploy reaches running processes within about a second plus start time.
    pub async fn watch(self: Arc<Self>) {
        let mut cursor: Option<i64> = None;
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            let url = match cursor {
                Some(c) => format!("{}/internal/v1/generations?after={c}", self.cfg.api_url),
                None => format!("{}/internal/v1/generations", self.cfg.api_url),
            };
            let resp = self.control.get(url).bearer_auth(&self.cfg.internal_token).send().await;
            let body: serde_json::Value = match resp.and_then(|r| r.error_for_status()) {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading account changes failed");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::debug!(error = %e, "polling account changes failed");
                    continue;
                }
            };
            for change in body["changes"].as_array().into_iter().flatten() {
                let (Some(account), Some(generation)) =
                    (change[0].as_str().and_then(|a| a.parse::<AccountId>().ok()), change[1].as_i64())
                else {
                    continue;
                };
                let running = self.accounts.lock().expect("poisoned").get(&account).cloned();
                let Some(state) = running else { continue };
                let stale = state.slot.lock().await.live.as_ref().is_some_and(|l| l.generation != generation);
                if stale {
                    let (me, account) = (self.clone(), account.clone());
                    tokio::spawn(async move {
                        if let Err(e) = me.rebuild(&account).await {
                            tracing::warn!(%account, error = %format!("{e:#}"), "applying a deploy failed");
                        }
                    });
                }
            }
            if let Some(c) = body["cursor"].as_i64() {
                cursor = Some(c);
            }
            if let Some(list) = body["tails"].as_array() {
                let set = list
                    .iter()
                    .filter_map(|t| {
                        Some((t[0].as_str()?.parse::<AccountId>().ok()?, t[1].as_str()?.to_owned()))
                    })
                    .collect();
                *self.tails.0.write().expect("poisoned") = set;
            }
        }
    }

    /// Checks the account's generation and starts a new process if it changed.
    /// While the control plane is unreachable, a running process keeps serving.
    async fn rebuild(self: &Arc<Self>, account: &AccountId) -> Result<Lookup> {
        let state = self.account(account);
        let _start = state.start.lock().await;
        let current = state.slot.lock().await.live.clone();
        let current = match current {
            Some(live) if live.alive().await => Some(live),
            _ => None,
        };
        match self.rebuild_locked(account, &state, current.clone()).await {
            Ok(lookup) => Ok(lookup),
            Err(e) => match current {
                Some(live) => {
                    tracing::warn!(%account, error = %format!("{e:#}"), "keeping the running generation");
                    state.slot.lock().await.checked_at = Some(Instant::now());
                    Ok(Lookup::Ready(live))
                }
                None => Err(e),
            },
        }
    }

    async fn rebuild_locked(
        self: &Arc<Self>,
        account: &AccountId,
        state: &AccountState,
        current: Option<Arc<Live>>,
    ) -> Result<Lookup> {
        let Some(generation) = self.generation(account).await? else {
            stop_current(state).await;
            return Ok(Lookup::NotFound);
        };
        if let Some(live) = &current {
            if live.generation == generation {
                state.slot.lock().await.checked_at = Some(Instant::now());
                return Ok(Lookup::Ready(live.clone()));
            }
        }

        let bundle = self.bundle(account).await?;
        if bundle.scripts.is_empty() && bundle.d1_databases.is_empty() {
            stop_current(state).await;
            state.slot.lock().await.checked_at = Some(Instant::now());
            return Ok(Lookup::NotFound);
        }

        let uid = self.uids.as_ref().map(|u| u.uid(account)).transpose()?;
        let account_dir = self.cfg.state_dir.join("a").join(account.as_str());
        for dir in [self.account_run_dir(account), account_dir.clone()] {
            std::fs::create_dir_all(&dir)?;
            if let Some(uid) = uid {
                super::uids::own(&dir, uid, 0o700)?;
            }
        }
        let (binding_socket, starts) = {
            let mut slot = state.slot.lock().await;
            if slot.binding_socket.is_none() {
                let p = self.account_run_dir(account).join("b.sock");
                super::bindings::serve(
                    self.cfg.clone(),
                    account.clone(),
                    &p,
                    self.tails.clone(),
                    self.http.clone(),
                    uid,
                )?;
                slot.binding_socket = Some(p);
            }
            slot.starts += 1;
            (slot.binding_socket.clone().expect("set above"), slot.starts)
        };

        let has_do = selvedge_workerd::render::uses_durable_objects(&bundle);
        let cluster = if has_do { self.cluster_options(account) } else { None };
        if has_do && cluster.is_none() && !self.cfg.allow_local_durable_objects {
            bail!(
                "account {account} uses Durable Objects, but this agent has no cluster bucket configured \
                 (set --do-bucket, or --allow-local-durable-objects on a single node)"
            );
        }
        let local_do = has_do && cluster.is_none();
        if local_do {
            // Two processes cannot share local Durable Object files.
            let old = state.slot.lock().await.live.take();
            if let Some(old) = old {
                old.stop(DRAIN_GRACE).await;
            }
        }

        let tag = format!("g{}-{starts}", bundle.generation);
        let do_dir =
            if local_do { account_dir.join("do-local") } else { account_dir.join(format!("do-{tag}")) };
        let opts = RenderOptions {
            dir: account_dir.join(&tag),
            socket_dir: self.account_run_dir(account).join(&tag),
            binding_socket,
            max_compatibility_date: self.max_date.clone(),
            do_storage_dir: do_dir.clone(),
            cluster,
        };
        self.fetch_blobs(&bundle).await?;
        let blob_dir = self.cfg.state_dir.join("blobs");
        let source = move |sha: &str| -> Result<Vec<u8>> {
            std::fs::read(blob_dir.join(sha)).with_context(|| format!("blob {sha}"))
        };
        let rendered = render(&bundle, &opts, &source)?;
        if let Some(uid) = uid {
            std::fs::create_dir_all(&opts.socket_dir)?;
            super::uids::own(&opts.socket_dir, uid, 0o700)?;
            super::uids::own_tree(&opts.dir, uid)?;
            if do_dir.exists() {
                super::uids::own_tree(&do_dir, uid)?;
            }
        }
        let label = format!("{account}/{tag}");
        let process = WorkerdProcess::start_as(
            &self.cfg.workerd,
            &rendered,
            &self.workerd_env(account),
            &label,
            READY_TIMEOUT,
            uid,
        )
        .await?;
        if let (Some(dir), Some(limits), Some(pid)) = (&self.cgroup_dir, &self.cfg.limits, process.pid()) {
            if let Err(e) = super::limits::place(dir, account, pid, limits) {
                tracing::warn!(%account, error = %e, "placing the account process in its cgroup failed");
            }
        }
        tracing::info!(%account, generation = bundle.generation, "account process ready");
        let live = Arc::new(Live {
            generation: bundle.generation,
            sockets: rendered.sockets.iter().cloned().collect(),
            splits: rendered.splits.iter().map(|(s, p, v)| (s.clone(), (*p, v.clone()))).collect(),
            process: Mutex::new(Some(process)),
            render_dir: opts.dir.clone(),
            socket_dir: opts.socket_dir.clone(),
            do_dir: (!local_do).then_some(do_dir),
        });
        let old = {
            let mut slot = state.slot.lock().await;
            slot.checked_at = Some(Instant::now());
            slot.live.replace(live.clone())
        };
        if let Some(old) = old {
            tokio::spawn(async move { old.stop(DRAIN_GRACE).await });
        }
        Ok(Lookup::Ready(live))
    }

    /// Unix socket paths are limited to ~107 bytes, so the run directory uses a
    /// short prefix of the account id.
    fn account_run_dir(&self, account: &AccountId) -> PathBuf {
        self.cfg.run_dir.join(&account.as_str()[..12])
    }

    fn cluster_options(&self, account: &AccountId) -> Option<ClusterOptions> {
        let c = self.cfg.cluster.as_ref()?;
        // Every agent derives the same per-account key from the shared secret.
        let mut h = Sha256::new();
        h.update(b"selvedge cluster key v1\0");
        h.update(c.secret.as_bytes());
        h.update(account.as_str().as_bytes());
        Some(ClusterOptions {
            bucket: format!("{}/{}", c.bucket.trim_end_matches('/'), account),
            // workerd reaches the store only through the agent's proxy.
            bucket_options: c
                .bucket_options
                .iter()
                .filter(|o| !super::UPSTREAM_OPTIONS.contains(&o.split('=').next().unwrap_or_default()))
                .cloned()
                .chain(self.s3_proxy.map(|a| format!("aws_endpoint=http://{a}")))
                .collect(),
            address: format!("{}:0", c.bind_host),
            advertise: None,
            key_hex: hex::encode(h.finalize()),
        })
    }

    /// A GET to the API, retried when the connection fails or times out. Each
    /// attempt is a new connection, so it can reach another API replica while
    /// a dead one is still listed in the Service's endpoints.
    async fn get(&self, client: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match client.get(url).bearer_auth(&self.cfg.internal_token).send().await {
                Ok(r) => return Ok(r),
                Err(e) if attempt < 4 && (e.is_connect() || e.is_timeout()) => {
                    tracing::debug!(%url, attempt, error = %e, "API request failed; retrying");
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    async fn generation(&self, account: &AccountId) -> Result<Option<i64>> {
        let url = format!("{}/internal/v1/accounts/{account}/generation", self.cfg.api_url);
        let resp = self.get(&self.control, &url).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let v: serde_json::Value = resp.error_for_status()?.json().await?;
        Ok(v["generation"].as_i64())
    }

    async fn bundle(&self, account: &AccountId) -> Result<AccountBundle> {
        let url = format!("{}/internal/v1/accounts/{account}/bundle", self.cfg.api_url);
        Ok(self.get(&self.http, &url).await?.error_for_status()?.json().await?)
    }

    async fn fetch_blobs(&self, bundle: &AccountBundle) -> Result<()> {
        let dir = self.cfg.state_dir.join("blobs");
        std::fs::create_dir_all(&dir)?;
        let modules = bundle.scripts.iter().flat_map(|s| &s.modules).map(|m| &m.sha256);
        let manifests = bundle.scripts.iter().filter_map(|s| s.assets.as_ref()).map(|a| &a.manifest);
        for sha in modules.chain(manifests) {
            let path = dir.join(sha);
            if path.exists() {
                continue;
            }
            let url = format!("{}/internal/v1/blobs/{sha}", self.cfg.api_url);
            let bytes = self.get(&self.http, &url).await?.error_for_status()?.bytes().await?;
            anyhow::ensure!(selvedge_core::sha256_hex(&bytes) == *sha, "blob {sha} failed its hash check");
            write_atomic(&path, &bytes)?;
        }
        Ok(())
    }

    /// Every running account process: `(account, pid)`.
    pub async fn live_pids(&self) -> Vec<(AccountId, u32)> {
        let states: Vec<_> =
            self.accounts.lock().expect("poisoned").iter().map(|(a, s)| (a.clone(), s.clone())).collect();
        let mut out = Vec::new();
        for (account, state) in states {
            let live = state.slot.lock().await.live.clone();
            if let Some(live) = live
                && let Some(pid) = live.process.lock().await.as_ref().and_then(WorkerdProcess::pid)
            {
                out.push((account, pid));
            }
        }
        out
    }

    /// Restarts an account's process that went over its limits: SIGTERM, then
    /// SIGKILL after [`LIMIT_GRACE`]; the next request starts a new one. In
    /// cluster mode the drain deregisters the instance, so its Durable Objects
    /// move at once instead of after the cluster lease (20 seconds). Starts
    /// wait meanwhile, so a new process never shares local SQLite files with
    /// the old one.
    pub async fn restart_over_limit(&self, account: &AccountId) {
        let state = self.accounts.lock().expect("poisoned").get(account).cloned();
        if let Some(state) = state {
            let _start = state.start.lock().await;
            let live = state.slot.lock().await.live.take();
            if let Some(live) = live {
                live.stop(LIMIT_GRACE).await;
            }
        }
    }

    /// Stops processes unused for longer than the idle timeout.
    pub async fn evict_idle(&self) {
        let idle: Vec<AccountId> = self
            .last_used
            .lock()
            .expect("poisoned")
            .iter()
            .filter(|(_, t)| t.elapsed() > self.cfg.idle_timeout)
            .map(|(a, _)| a.clone())
            .collect();
        for account in idle {
            let state = self.account(&account);
            let _start = state.start.lock().await;
            let mut slot = state.slot.lock().await;
            // Re-check under the lock: a request may have just used it.
            let still_idle = self
                .last_used
                .lock()
                .expect("poisoned")
                .get(&account)
                .is_some_and(|t| t.elapsed() > self.cfg.idle_timeout);
            if still_idle {
                if let Some(live) = slot.live.take() {
                    tracing::info!(%account, "stopping idle account process");
                    live.stop(DRAIN_GRACE).await;
                }
                self.last_used.lock().expect("poisoned").remove(&account);
            }
        }
    }

    /// Stops every process (agent shutdown).
    pub async fn stop_all(&self) {
        let states: Vec<_> = self.accounts.lock().expect("poisoned").values().cloned().collect();
        let mut stops = Vec::new();
        for state in states {
            if let Some(live) = state.slot.lock().await.live.take() {
                stops.push(tokio::spawn(async move { live.stop(DRAIN_GRACE).await }));
            }
        }
        for s in stops {
            let _ = s.await;
        }
    }
}

/// The only variables workerd gets: what it needs to reach the cluster bucket,
/// and a sane PATH/locale. Never the internal token or database URL.
impl Manager {
    /// workerd's environment: nothing of the agent's, and for Durable Object
    /// storage only credentials that the agent's S3 proxy accepts for this
    /// account.
    fn workerd_env(&self, account: &AccountId) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> =
            ["TZ"].iter().filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v))).collect();
        env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
        if let (Some(c), Some(_)) = (&self.cfg.cluster, self.s3_proxy) {
            env.push(("AWS_ACCESS_KEY_ID".into(), account.to_string()));
            env.push(("AWS_SECRET_ACCESS_KEY".into(), super::s3proxy::account_secret(&c.secret, account)));
        }
        env
    }
}

async fn stop_current(state: &AccountState) {
    let old = state.slot.lock().await.live.take();
    if let Some(old) = old {
        tokio::spawn(async move { old.stop(DRAIN_GRACE).await });
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
