//! Instances as Docker containers, for a single node (`selvedge all`) and
//! for tests. Containers share the host's kernel: this backend sandboxes
//! nothing, and suits an installation that runs only its operator's images.
//!
//! An instance is two containers, as workerd's own Docker backend makes
//! them: the sidecar, which owns the network namespace, and the application
//! in it. The difference is who decides what runs: here, the controller.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use serde_json::{Value, json};

use super::spec::{self, CA_DIR, CA_FILE, GATEWAY_HOST, INGRESS_PORT};
use super::{Backend, Instance, InstanceSpec, Phase, Runner};

const LABEL_ID: &str = "selvedge.dev/instance-id";
const LABEL_ROLE: &str = "selvedge.dev/role";

#[derive(Clone, Debug)]
pub struct DockerSettings {
    pub socket: PathBuf,
    pub sidecar_image: String,
    pub dns: Vec<String>,
    /// Distinguishes this installation's containers from another's on the
    /// same daemon.
    pub instance: String,
}

pub struct DockerBackend {
    inner: Arc<Inner>,
}

struct Inner {
    settings: DockerSettings,
    http: reqwest::Client,
    /// Instances whose start failed, and why.
    failures: Mutex<HashMap<String, String>>,
}

impl DockerBackend {
    pub fn new(settings: DockerSettings) -> Self {
        // The sidecar closes a connection after each answer.
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("HTTP client");
        Self { inner: Arc::new(Inner { settings, http, failures: Mutex::new(HashMap::new()) }) }
    }
}

fn proxy_name(id: &str) -> String {
    format!("{id}-proxy")
}

/// A `ustar` archive of one file under directories, as Docker's archive
/// endpoint takes it. `path` is relative, like `cloudflare/certs/ca.crt`.
pub fn tar_file(path: &str, content: &[u8]) -> Vec<u8> {
    fn header(name: &str, size: usize, kind: u8, mode: &str) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(mode.as_bytes());
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[148..156].copy_from_slice(b"        ");
        h[156] = kind;
        h[257..265].copy_from_slice(b"ustar\x0000");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h
    }
    let mut out = Vec::new();
    let mut dir = String::new();
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        dir.push_str(part);
        dir.push('/');
        out.extend(header(&dir, 0, b'5', "0000755\0"));
    }
    out.extend(header(path, content.len(), b'0', "0000644\0"));
    out.extend(content);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out.resize(out.len() + 1024, 0);
    out
}

impl Inner {
    /// One request to the Docker daemon; the whole response is read.
    async fn docker(
        &self,
        method: http::Method,
        path: &str,
        body: Option<(&str, Bytes)>,
        headers: &[(&str, String)],
    ) -> Result<(u16, Bytes)> {
        let stream = tokio::net::UnixStream::connect(&self.settings.socket)
            .await
            .with_context(|| format!("connecting to Docker at {}", self.settings.socket.display()))?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = http::Request::builder().method(method.clone()).uri(path).header("host", "docker");
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let req = match body {
            Some((content_type, bytes)) => req
                .header("content-type", content_type)
                .header("content-length", bytes.len())
                .body(Full::new(bytes))?,
            None => req.header("content-length", 0).body(Full::new(Bytes::new()))?,
        };
        let resp = sender.send_request(req).await.with_context(|| format!("{method} {path}"))?;
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await?.to_bytes();
        Ok((status, body))
    }

    async fn docker_json(
        &self,
        method: http::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let body = body.map(|b| ("application/json", Bytes::from(b.to_string())));
        let (status, bytes) = self.docker(method, path, body, &[]).await?;
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    /// Makes an image available, pulling it if the daemon does not have it.
    async fn ensure_image(&self, image: &str, auth: Option<&(String, String, String)>) -> Result<()> {
        let encoded: String = url::form_urlencoded::byte_serialize(image.as_bytes()).collect();
        if self.docker(http::Method::GET, &format!("/images/{encoded}/json"), None, &[]).await?.0 == 200 {
            return Ok(());
        }
        use base64::Engine;
        let headers: Vec<(&str, String)> = auth
            .map(|(host, user, password)| {
                let cfg = json!({ "username": user, "password": password, "serveraddress": host });
                ("x-registry-auth", base64::engine::general_purpose::URL_SAFE.encode(cfg.to_string()))
            })
            .into_iter()
            .collect();
        let (status, body) = self
            .docker(http::Method::POST, &format!("/images/create?fromImage={encoded}"), None, &headers)
            .await?;
        // The pull's progress is a stream of JSON lines; a failure is one of them.
        let text = String::from_utf8_lossy(&body);
        let failed = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find_map(|l| l.get("error").or(l.get("message")).and_then(Value::as_str).map(str::to_owned));
        if status != 200 || failed.is_some() {
            bail!("pulling {image}: {}", failed.unwrap_or_else(|| format!("status {status}")));
        }
        Ok(())
    }

    fn labels(&self, s: &InstanceSpec, role: &str) -> Value {
        let mut l = spec::labels(s, &self.settings.instance);
        l[LABEL_ID] = json!(s.id);
        l[LABEL_ROLE] = json!(role);
        l
    }

    async fn sidecar_ip(&self, id: &str) -> Result<IpAddr> {
        let (status, v) = self
            .docker_json(http::Method::GET, &format!("/containers/{}/json", proxy_name(id)), None)
            .await?;
        if status != 200 {
            bail!("inspecting the sidecar: status {status}");
        }
        v.pointer("/NetworkSettings/Networks/bridge/IPAddress")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .context("the sidecar has no address on the bridge network")
    }

    /// Creates and starts the sidecar. `false` if the instance already exists.
    async fn start_sidecar(&self, s: &InstanceSpec) -> Result<bool> {
        self.ensure_image(&self.settings.sidecar_image, None).await?;
        let (_, bridge) = self.docker_json(http::Method::GET, "/networks/bridge", None).await?;
        let cidr = bridge.pointer("/IPAM/Config/0/Subnet").and_then(Value::as_str).unwrap_or("172.17.0.0/16");
        let body = json!({
            "Image": self.settings.sidecar_image,
            "Cmd": spec::sidecar_args(s.egress_port, cidr),
            "Labels": self.labels(s, "proxy"),
            "HostConfig": {
                "NetworkMode": "bridge",
                "CapAdd": ["NET_ADMIN"],
                "ExtraHosts": [format!("{GATEWAY_HOST}:{}", s.runner.gateway)],
                "Dns": self.settings.dns,
                "Memory": 128 << 20,
            },
        });
        let name = proxy_name(&s.id);
        match self
            .docker_json(http::Method::POST, &format!("/containers/create?name={name}"), Some(&body))
            .await?
        {
            (201, _) => {}
            (409, _) => return Ok(false),
            (status, v) => {
                bail!("creating the sidecar: {status} {}", v["message"].as_str().unwrap_or_default())
            }
        }
        let (status, v) =
            self.docker_json(http::Method::POST, &format!("/containers/{name}/start"), None).await?;
        if status != 204 {
            bail!("starting the sidecar: {status} {}", v["message"].as_str().unwrap_or_default());
        }
        Ok(true)
    }

    /// The rest of an instance's start, after its sidecar: configure egress,
    /// fetch the CA, then create and start the application with the CA in it.
    async fn start_app(&self, s: &InstanceSpec) -> Result<()> {
        let ip = self.sidecar_ip(&s.id).await?;
        let sidecar = format!("http://{}", std::net::SocketAddr::new(ip, INGRESS_PORT));
        let mut attempt = 0;
        loop {
            attempt += 1;
            let sent = self.http.put(format!("{sidecar}/egress")).json(&s.egress).send().await;
            match sent.map(|r| r.status().is_success()) {
                Ok(true) => break,
                _ if attempt < 100 => tokio::time::sleep(Duration::from_millis(100)).await,
                Ok(false) => bail!("the sidecar refused the egress configuration"),
                Err(e) => bail!("the sidecar did not start listening: {e}"),
            }
        }
        let ca = self.http.get(format!("{sidecar}/ca")).send().await?.error_for_status()?.bytes().await?;
        self.ensure_image(&s.image, Some(&s.pull)).await?;
        let mut body = json!({
            "Image": s.image,
            "Env": s.env,
            "Labels": self.labels(s, "app"),
            "HostConfig": {
                "NetworkMode": format!("container:{}", proxy_name(&s.id)),
                "Memory": s.resources.memory_mib << 20,
                "NanoCpus": (s.resources.vcpu * 1e9) as u64,
                "RestartPolicy": { "Name": "no" },
            },
        });
        if let Some(command) = &s.command {
            body["Cmd"] = json!(command);
        }
        match self
            .docker_json(http::Method::POST, &format!("/containers/create?name={}", s.id), Some(&body))
            .await?
        {
            (201 | 409, _) => {}
            (status, v) => {
                bail!("creating the container: {status} {}", v["message"].as_str().unwrap_or_default())
            }
        }
        let dir = CA_DIR.trim_start_matches("/etc/");
        let tar = tar_file(&format!("{dir}/{CA_FILE}"), &ca);
        let (status, _) = self
            .docker(
                http::Method::PUT,
                &format!("/containers/{}/archive?path=/etc", s.id),
                Some(("application/x-tar", Bytes::from(tar))),
                &[],
            )
            .await?;
        if status != 200 {
            tracing::warn!(instance = %s.id, status, "writing the sidecar's CA into the container failed");
        }
        let (status, v) =
            self.docker_json(http::Method::POST, &format!("/containers/{}/start", s.id), None).await?;
        if status != 204 && status != 304 {
            bail!("starting the container: {status} {}", v["message"].as_str().unwrap_or_default());
        }
        Ok(())
    }
}

/// The exit code in a listing's status text, `Exited (3) 5 seconds ago`.
fn exit_code(status: &str) -> Option<i32> {
    status.split_once('(')?.1.split_once(')')?.0.parse().ok()
}

#[async_trait::async_trait]
impl Backend for DockerBackend {
    async fn list(&self) -> Result<Vec<Instance>> {
        let filters = json!({ "label": [
            format!("{}={}", spec::LABEL_COMPONENT, spec::COMPONENT),
            format!("app.kubernetes.io/instance={}", self.inner.settings.instance),
        ] });
        let encoded: String = url::form_urlencoded::byte_serialize(filters.to_string().as_bytes()).collect();
        let (status, v) = self
            .inner
            .docker_json(http::Method::GET, &format!("/containers/json?all=true&filters={encoded}"), None)
            .await?;
        if status != 200 {
            bail!("listing containers: status {status}");
        }
        let now = crate::registry::now() as i64;
        let failures = self.inner.failures.lock().expect("poisoned").clone();
        let mut out: HashMap<String, Instance> = HashMap::new();
        let all = v.as_array().cloned().unwrap_or_default();
        let label = |c: &Value, k: &str| c["Labels"][k].as_str().unwrap_or_default().to_owned();
        // Sidecars first: an instance exists from its sidecar's creation.
        for c in all.iter().filter(|c| c["Labels"][LABEL_ROLE] == "proxy") {
            let id = label(c, LABEL_ID);
            let failed = failures.get(&id).cloned();
            out.insert(
                id.clone(),
                Instance {
                    id,
                    account: label(c, spec::LABEL_ACCOUNT),
                    application: label(c, spec::LABEL_APPLICATION),
                    session: label(c, spec::LABEL_SESSION),
                    runner: label(c, spec::LABEL_RUNNER),
                    phase: if failed.is_some() { Phase::Failed } else { Phase::Pending },
                    ip: c
                        .pointer("/NetworkSettings/Networks/bridge/IPAddress")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse().ok()),
                    exit_code: None,
                    message: failed,
                    terminating: false,
                    age_secs: (now - c["Created"].as_i64().unwrap_or(now)).max(0) as u64,
                },
            );
        }
        for c in all.iter().filter(|c| c["Labels"][LABEL_ROLE] == "app") {
            let Some(i) = out.get_mut(&label(c, LABEL_ID)) else { continue };
            match c["State"].as_str() {
                Some("running" | "restarting" | "paused") if i.ip.is_some() => i.phase = Phase::Running,
                Some("exited" | "dead") => {
                    i.phase = Phase::Exited;
                    i.exit_code = Some(c["Status"].as_str().and_then(exit_code).unwrap_or(1));
                }
                _ => {}
            }
        }
        Ok(out.into_values().collect())
    }

    async fn create(&self, s: &InstanceSpec) -> Result<()> {
        self.inner.failures.lock().expect("poisoned").remove(&s.id);
        if !self.inner.start_sidecar(s).await? {
            return Ok(());
        }
        // The rest takes seconds (an image pull, the sidecar's start): the
        // instance is listed as pending meanwhile.
        let (inner, s) = (self.inner.clone(), s.clone());
        tokio::spawn(async move {
            if let Err(e) = inner.start_app(&s).await {
                tracing::warn!(instance = %s.id, error = %format!("{e:#}"), "starting a container instance failed");
                inner.failures.lock().expect("poisoned").insert(s.id.clone(), format!("{e:#}"));
            }
        });
        Ok(())
    }

    async fn delete(&self, id: &str, grace: u32) -> Result<()> {
        if !id.starts_with("slvc-") || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            bail!("not an instance id");
        }
        if grace > 0 {
            let _ = self
                .inner
                .docker(http::Method::POST, &format!("/containers/{id}/stop?t={grace}"), None, &[])
                .await;
        }
        for name in [id.to_owned(), proxy_name(id)] {
            let (status, body) = self
                .inner
                .docker(http::Method::DELETE, &format!("/containers/{name}?force=true&v=true"), None, &[])
                .await?;
            if !matches!(status, 204 | 404 | 409) {
                bail!("removing {name}: {status} {}", String::from_utf8_lossy(&body));
            }
        }
        self.inner.failures.lock().expect("poisoned").remove(id);
        Ok(())
    }

    async fn runner(&self, name: &str, _peer: Option<IpAddr>) -> Result<Option<Runner>> {
        // One node: the agent is on the Docker host, which containers reach
        // at the bridge's gateway.
        Ok(Some(Runner { name: name.to_owned(), gateway: "host-gateway".to_owned(), uid: None }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_layout() {
        let t = tar_file("cloudflare/certs/ca.crt", b"hello");
        // Two directories, the file's header, one block of content, the end.
        assert_eq!(t.len(), 512 * 4 + 1024);
        assert_eq!(&t[..11], b"cloudflare/");
        assert_eq!(t[156], b'5');
        assert_eq!(&t[1024..1047], b"cloudflare/certs/ca.crt");
        assert_eq!(&t[1024 + 124..1024 + 136], b"00000000005\0");
        assert_eq!(&t[1536..1541], b"hello");
        // The checksum is the header's bytes summed with the field as spaces.
        let mut h = t[1024..1536].to_vec();
        let stored = u32::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
        h[148..156].copy_from_slice(b"        ");
        assert_eq!(stored, h.iter().map(|b| u32::from(*b)).sum::<u32>());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code("Exited (3) 5 seconds ago"), Some(3));
        assert_eq!(exit_code("Up 2 minutes"), None);
    }
}
