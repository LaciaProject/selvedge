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
use selvedge_core::{AccountBundle, AccountId, Binding};
use selvedge_workerd::{ClusterOptions, RenderOptions, WorkerdProcess, render};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::AgentConfig;

/// How long a known generation is trusted before asking the API again. This
/// bounds how long a deploy takes to reach a running process.
const GENERATION_TTL: Duration = Duration::from_secs(2);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_GRACE: Duration = Duration::from_secs(30);

/// A running account process.
pub struct Live {
    pub generation: i64,
    pub sockets: HashMap<String, PathBuf>,
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
}

pub struct Manager {
    cfg: Arc<AgentConfig>,
    http: reqwest::Client,
    max_date: String,
    slots: StdMutex<HashMap<AccountId, Arc<Mutex<Slot>>>>,
    last_used: StdMutex<HashMap<AccountId, Instant>>,
}

pub enum Lookup {
    Ready(Arc<Live>),
    /// The account does not exist or has no scripts.
    NotFound,
}

impl Manager {
    pub fn new(cfg: Arc<AgentConfig>, max_date: String) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            http: reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("HTTP client"),
            max_date,
            slots: StdMutex::new(HashMap::new()),
            last_used: StdMutex::new(HashMap::new()),
        })
    }

    fn slot(&self, account: &AccountId) -> Arc<Mutex<Slot>> {
        self.slots.lock().expect("poisoned").entry(account.clone()).or_default().clone()
    }

    /// The account's process, started or replaced if needed.
    pub async fn ensure(self: &Arc<Self>, account: &AccountId) -> Result<Lookup> {
        self.last_used.lock().expect("poisoned").insert(account.clone(), Instant::now());
        let slot = self.slot(account);
        let mut slot = slot.lock().await;

        if let Some(live) = &slot.live {
            let fresh = slot.checked_at.is_some_and(|t| t.elapsed() < GENERATION_TTL);
            if fresh && live.alive().await {
                return Ok(Lookup::Ready(live.clone()));
            }
        }

        let generation = match self.generation(account).await {
            Ok(Some(g)) => g,
            Ok(None) => {
                if let Some(old) = slot.live.take() {
                    tokio::spawn(async move { old.stop(DRAIN_GRACE).await });
                }
                return Ok(Lookup::NotFound);
            }
            Err(e) => {
                // The control plane is down: keep serving what we have.
                if let Some(live) = &slot.live {
                    if live.alive().await {
                        tracing::warn!(%account, error = %format!("{e:#}"), "API unreachable; serving cached generation");
                        return Ok(Lookup::Ready(live.clone()));
                    }
                }
                return Err(e);
            }
        };

        if let Some(live) = slot.live.clone() {
            if live.generation == generation && live.alive().await {
                slot.checked_at = Some(Instant::now());
                return Ok(Lookup::Ready(live));
            }
        }

        let bundle = self.bundle(account).await?;
        if bundle.scripts.is_empty() {
            if let Some(old) = slot.live.take() {
                tokio::spawn(async move { old.stop(DRAIN_GRACE).await });
            }
            slot.checked_at = Some(Instant::now());
            return Ok(Lookup::NotFound);
        }

        let binding_socket = match &slot.binding_socket {
            Some(p) => p.clone(),
            None => {
                let p = self.account_run_dir(account).join("b.sock");
                super::bindings::serve(self.cfg.clone(), account.clone(), &p)?;
                slot.binding_socket = Some(p.clone());
                p
            }
        };

        let has_do = bundle
            .scripts
            .iter()
            .any(|s| s.bindings.iter().any(|b| matches!(b, Binding::DurableObjectNamespace { .. })));
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
            if let Some(old) = slot.live.take() {
                old.stop(DRAIN_GRACE).await;
            }
        }

        slot.starts += 1;
        let tag = format!("g{}-{}", bundle.generation, slot.starts);
        let account_dir = self.cfg.state_dir.join("a").join(account.as_str());
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
        let label = format!("{account}/{tag}");
        let process =
            WorkerdProcess::start(&self.cfg.workerd, &rendered, &workerd_env(), &label, READY_TIMEOUT)
                .await?;
        tracing::info!(%account, generation = bundle.generation, "account process ready");
        let live = Arc::new(Live {
            generation: bundle.generation,
            sockets: rendered.sockets.iter().cloned().collect(),
            process: Mutex::new(Some(process)),
            render_dir: opts.dir.clone(),
            socket_dir: opts.socket_dir.clone(),
            do_dir: (!local_do).then_some(do_dir),
        });
        if let Some(old) = slot.live.replace(live.clone()) {
            tokio::spawn(async move { old.stop(DRAIN_GRACE).await });
        }
        slot.checked_at = Some(Instant::now());
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
            bucket_options: c.bucket_options.clone(),
            address: format!("{}:0", c.bind_host),
            advertise: None,
            key_hex: hex::encode(h.finalize()),
        })
    }

    async fn generation(&self, account: &AccountId) -> Result<Option<i64>> {
        let resp = self
            .http
            .get(format!("{}/internal/v1/accounts/{account}/generation", self.cfg.api_url))
            .bearer_auth(&self.cfg.internal_token)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let v: serde_json::Value = resp.error_for_status()?.json().await?;
        Ok(v["generation"].as_i64())
    }

    async fn bundle(&self, account: &AccountId) -> Result<AccountBundle> {
        Ok(self
            .http
            .get(format!("{}/internal/v1/accounts/{account}/bundle", self.cfg.api_url))
            .bearer_auth(&self.cfg.internal_token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn fetch_blobs(&self, bundle: &AccountBundle) -> Result<()> {
        let dir = self.cfg.state_dir.join("blobs");
        std::fs::create_dir_all(&dir)?;
        for m in bundle.scripts.iter().flat_map(|s| &s.modules) {
            let path = dir.join(&m.sha256);
            if path.exists() {
                continue;
            }
            let bytes = self
                .http
                .get(format!("{}/internal/v1/blobs/{}", self.cfg.api_url, m.sha256))
                .bearer_auth(&self.cfg.internal_token)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            anyhow::ensure!(
                selvedge_core::sha256_hex(&bytes) == m.sha256,
                "blob {} failed its hash check",
                m.sha256
            );
            write_atomic(&path, &bytes)?;
        }
        Ok(())
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
            let slot = self.slot(&account);
            let mut slot = slot.lock().await;
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
        let slots: Vec<_> = self.slots.lock().expect("poisoned").values().cloned().collect();
        let mut stops = Vec::new();
        for slot in slots {
            if let Some(live) = slot.lock().await.live.take() {
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
fn workerd_env() -> Vec<(String, String)> {
    const PASS: &[&str] = &[
        "AWS_ENDPOINT_URL",
        "AWS_REGION",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_CREDENTIALS_FILE",
        "TZ",
    ];
    let mut env: Vec<(String, String)> =
        PASS.iter().filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v))).collect();
    env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
    env
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
