//! Runs one workerd process for one rendered configuration.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::render::Rendered;

/// A running workerd process. Dropping it kills the process.
pub struct WorkerdProcess {
    child: Child,
    pub sockets: Vec<(String, PathBuf)>,
    label: String,
}

impl WorkerdProcess {
    /// Starts workerd on a rendered config and waits until every script socket
    /// accepts connections.
    /// workerd starts with an empty environment plus `env`: the agent's own
    /// environment holds platform secrets that tenant code must never reach,
    /// even through a sandbox escape.
    pub async fn start(
        workerd: &Path,
        rendered: &Rendered,
        env: &[(String, String)],
        label: &str,
        ready_timeout: Duration,
    ) -> Result<Self> {
        Self::start_as(workerd, rendered, env, label, ready_timeout, None).await
    }

    /// Like [`start`](Self::start), as user and group `uid` when given (the
    /// caller must be allowed to switch to it). Supplementary groups are
    /// dropped and, as a non-root user, workerd has no capabilities.
    pub async fn start_as(
        workerd: &Path,
        rendered: &Rendered,
        env: &[(String, String)],
        label: &str,
        ready_timeout: Duration,
        uid: Option<u32>,
    ) -> Result<Self> {
        for (_, socket) in &rendered.sockets {
            if let Some(parent) = socket.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(socket);
        }
        let mut cmd = Command::new(workerd);
        cmd.arg("serve");
        if rendered.experimental {
            cmd.arg("--experimental");
        }
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
        if let Some(uid) = uid {
            // std drops supplementary groups (setgroups) when running as root.
            cmd.uid(uid).gid(uid);
        }
        cmd.arg(&rendered.config_path)
            .current_dir(rendered.config_path.parent().unwrap_or(Path::new(".")))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("starting {}", workerd.display()))?;
        forward_logs(&mut child, label);

        let mut process = Self { child, sockets: rendered.sockets.clone(), label: label.to_owned() };
        let deadline = tokio::time::Instant::now() + ready_timeout;
        loop {
            if let Some(status) = process.child.try_wait()? {
                bail!("workerd for {label} exited during startup: {status}");
            }
            if process.ready().await {
                return Ok(process);
            }
            if tokio::time::Instant::now() > deadline {
                bail!("workerd for {label} was not ready within {ready_timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn ready(&self) -> bool {
        for (_, socket) in &self.sockets {
            if tokio::net::UnixStream::connect(socket).await.is_err() {
                return false;
            }
        }
        true
    }

    pub fn socket(&self, script: &str) -> Option<&Path> {
        self.sockets.iter().find(|(s, _)| s == script).map(|(_, p)| p.as_path())
    }

    /// Whether the process is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Asks workerd to drain (SIGTERM), then kills it after `grace`. In cluster
    /// mode a drain deregisters the instance so peers take its objects at once.
    pub async fn stop(mut self, grace: Duration) {
        if let Some(pid) = self.child.id() {
            let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status().await;
        }
        if tokio::time::timeout(grace, self.child.wait()).await.is_err() {
            tracing::warn!(label = %self.label, "workerd did not drain in time; killing");
            let _ = self.child.kill().await;
        }
    }
}

fn forward_logs(child: &mut Child, label: &str) {
    if let Some(out) = child.stdout.take() {
        let label = label.to_owned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "workerd", label = %label, "{line}");
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let label = label.to_owned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "workerd", label = %label, "{line}");
            }
        });
    }
}
