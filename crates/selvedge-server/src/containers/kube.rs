//! Instances as Kubernetes pods.
//!
//! Talks to the API server directly with the pod's service account: the
//! controller needs five calls on pods and secrets in its own namespace,
//! which does not justify a client library.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::spec::{self, PodSettings};
use super::{Backend, Instance, InstanceSpec, Phase, Runner};

const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub struct KubeBackend {
    http: reqwest::Client,
    /// `https://host:port`.
    server: String,
    namespace: String,
    /// Re-read for every request: the kubelet rotates the token.
    token: PathBuf,
    settings: PodSettings,
    /// The label selector for runner pods; a caller must be one of them.
    runner_selector: Vec<(String, String)>,
}

/// Waiting reasons that will not go away on their own.
const FATAL_WAITING: &[&str] =
    &["ImagePullBackOff", "InvalidImageName", "CreateContainerConfigError", "ErrImageNeverPull"];

impl KubeBackend {
    /// From inside a pod: the service account's token, CA and namespace.
    pub fn in_cluster(settings: PodSettings, runner_selector: Vec<(String, String)>) -> Result<Self> {
        let dir = PathBuf::from(SERVICE_ACCOUNT);
        let host = std::env::var("KUBERNETES_SERVICE_HOST")
            .context("not running in a Kubernetes pod (KUBERNETES_SERVICE_HOST is not set)")?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
        let host = if host.contains(':') { format!("[{host}]") } else { host };
        let ca = std::fs::read(dir.join("ca.crt")).context("reading the service account's CA")?;
        let namespace = std::fs::read_to_string(dir.join("namespace"))
            .context("reading the service account's namespace")?
            .trim()
            .to_owned();
        let http = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(&ca)?)
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            server: format!("https://{host}:{port}"),
            namespace,
            token: dir.join("token"),
            settings,
            runner_selector,
        })
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let token =
            tokio::fs::read_to_string(&self.token).await.context("reading the service account token")?;
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.server))
            .bearer_auth(token.trim())
            .header("accept", "application/json");
        if let Some(b) = body {
            let content_type = if method == reqwest::Method::PATCH {
                "application/merge-patch+json"
            } else {
                "application/json"
            };
            req = req.header("content-type", content_type).body(b.to_string());
        }
        let resp = req.send().await.with_context(|| format!("{method} {path}"))?;
        let status = resp.status().as_u16();
        let value = resp.json().await.unwrap_or(Value::Null);
        Ok((status, value))
    }

    fn pods(&self) -> String {
        format!("/api/v1/namespaces/{}/pods", self.namespace)
    }

    fn secrets(&self) -> String {
        format!("/api/v1/namespaces/{}/secrets", self.namespace)
    }

    async fn pod(&self, name: &str) -> Result<Option<Value>> {
        // A name from a request must not become another path.
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.') {
            return Ok(None);
        }
        match self.call(reqwest::Method::GET, &format!("{}/{name}", self.pods()), None).await? {
            (200, pod) => Ok(Some(pod)),
            (404, _) => Ok(None),
            (status, body) => bail!("reading pod {name}: {status} {}", message(&body)),
        }
    }
}

fn message(body: &Value) -> String {
    body.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_owned()
}

/// Whether a pod is gone for good: being deleted, or finished.
fn pod_is_dead(pod: &Value) -> bool {
    pod.pointer("/metadata/deletionTimestamp").is_some_and(|v| !v.is_null())
        || matches!(pod.pointer("/status/phase").and_then(Value::as_str), Some("Failed" | "Succeeded"))
}

/// The instance a pod is, as of `now` (Unix seconds).
pub fn instance_of(pod: &Value, now: i64) -> Option<Instance> {
    let meta = pod.get("metadata")?;
    let label = |k: &str| meta.pointer(&format!("/labels/{}", k.replace('/', "~1"))).and_then(Value::as_str);
    let status = pod.get("status").cloned().unwrap_or(Value::Null);
    let statuses = |key: &str| status.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let app = statuses("containerStatuses").into_iter().find(|c| c["name"] == "app");
    let ip = status.get("podIP").and_then(Value::as_str).and_then(|s| s.parse::<IpAddr>().ok());
    let pod_phase = status.get("phase").and_then(Value::as_str).unwrap_or("Pending");
    let terminated = app.as_ref().and_then(|a| a.pointer("/state/terminated")).filter(|t| t.is_object());
    let running = app.as_ref().and_then(|a| a.pointer("/state/running")).is_some_and(Value::is_object);
    // A waiting container that will never start, wherever in the pod it is.
    let stuck =
        statuses("initContainerStatuses").into_iter().chain(statuses("containerStatuses")).find_map(|c| {
            let w = c.pointer("/state/waiting")?;
            let reason = w.get("reason")?.as_str()?;
            FATAL_WAITING.contains(&reason).then(|| {
                format!("{reason}: {}", w.get("message").and_then(Value::as_str).unwrap_or_default())
            })
        });
    let unschedulable = status.get("conditions").and_then(Value::as_array).and_then(|c| {
        c.iter().find(|c| c["type"] == "PodScheduled" && c["status"] == "False").map(|c| {
            format!("not scheduled: {}", c.get("message").and_then(Value::as_str).unwrap_or_default())
        })
    });
    let (phase, exit_code, message) = if let Some(t) = terminated {
        let code = t.get("exitCode").and_then(Value::as_i64).unwrap_or(1) as i32;
        (Phase::Exited, Some(code), t.get("reason").and_then(Value::as_str).map(str::to_owned))
    } else if running && ip.is_some() {
        (Phase::Running, None, None)
    } else if pod_phase == "Failed" {
        let m = status.get("message").or(status.get("reason")).and_then(Value::as_str);
        (Phase::Failed, None, Some(m.unwrap_or("the pod failed before the application started").to_owned()))
    } else if pod_phase == "Succeeded" {
        (Phase::Exited, Some(0), None)
    } else if let Some(m) = stuck {
        (Phase::Failed, None, Some(m))
    } else {
        (Phase::Pending, None, unschedulable)
    };
    let created = meta
        .get("creationTimestamp")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(now, |t| t.timestamp());
    Some(Instance {
        id: meta.get("name")?.as_str()?.to_owned(),
        account: label(spec::LABEL_ACCOUNT)?.to_owned(),
        application: label(spec::LABEL_APPLICATION).unwrap_or_default().to_owned(),
        session: label(spec::LABEL_SESSION).unwrap_or_default().to_owned(),
        runner: label(spec::LABEL_RUNNER).unwrap_or_default().to_owned(),
        phase,
        ip,
        exit_code,
        message,
        terminating: meta.get("deletionTimestamp").is_some_and(|v| !v.is_null()),
        age_secs: (now - created).max(0) as u64,
    })
}

#[async_trait::async_trait]
impl Backend for KubeBackend {
    async fn list(&self) -> Result<Vec<Instance>> {
        let selector = format!(
            "{}={},app.kubernetes.io/instance={}",
            spec::LABEL_COMPONENT,
            spec::COMPONENT,
            self.settings.instance
        );
        let path =
            format!("{}?labelSelector={}", self.pods(), selector.replace('=', "%3D").replace(',', "%2C"));
        let (status, body) = self.call(reqwest::Method::GET, &path, None).await?;
        if status != 200 {
            bail!("listing pods: {status} {}", message(&body));
        }
        let now = crate::registry::now() as i64;
        Ok(body["items"].as_array().into_iter().flatten().filter_map(|p| instance_of(p, now)).collect())
    }

    async fn create(&self, s: &InstanceSpec) -> Result<()> {
        let secret = spec::pull_secret(s, &self.settings.instance);
        let name = spec::pull_secret_name(&s.id);
        match self.call(reqwest::Method::POST, &self.secrets(), Some(&secret)).await? {
            (201, _) => {}
            // Left by an earlier attempt: its credentials may have expired.
            (409, _) => {
                let (status, body) = self
                    .call(reqwest::Method::PUT, &format!("{}/{name}", self.secrets()), Some(&secret))
                    .await?;
                if status != 200 {
                    bail!("replacing the pull secret: {status} {}", message(&body));
                }
            }
            (status, body) => bail!("creating the pull secret: {status} {}", message(&body)),
        }
        let pod = spec::pod(s, &self.settings);
        let created = match self.call(reqwest::Method::POST, &self.pods(), Some(&pod)).await? {
            (201, created) => created,
            (409, _) => return Ok(()),
            (status, body) => {
                let _ = self.call(reqwest::Method::DELETE, &format!("{}/{name}", self.secrets()), None).await;
                bail!("creating the pod: {status} {}", message(&body));
            }
        };
        // The pod owns its pull secret, so the secret goes when it does.
        if let Some(uid) = created.pointer("/metadata/uid").and_then(Value::as_str) {
            let owner = json!({ "metadata": { "ownerReferences": [{
                "apiVersion": "v1", "kind": "Pod", "name": s.id, "uid": uid,
                "controller": false, "blockOwnerDeletion": false,
            }] } });
            let (status, body) = self
                .call(reqwest::Method::PATCH, &format!("{}/{name}", self.secrets()), Some(&owner))
                .await?;
            if status != 200 {
                tracing::warn!(instance = %s.id, status, message = %message(&body), "setting the pull secret's owner failed");
            }
        }
        Ok(())
    }

    async fn delete(&self, id: &str, grace: u32) -> Result<()> {
        if !id.starts_with("slvc-") || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            bail!("not an instance id");
        }
        let options = json!({ "gracePeriodSeconds": grace, "propagationPolicy": "Background" });
        match self.call(reqwest::Method::DELETE, &format!("{}/{id}", self.pods()), Some(&options)).await? {
            (200 | 202 | 404, _) => {}
            (status, body) => bail!("deleting pod {id}: {status} {}", message(&body)),
        }
        let secret = format!("{}/{}", self.secrets(), spec::pull_secret_name(id));
        let _ = self.call(reqwest::Method::DELETE, &secret, None).await;
        Ok(())
    }

    async fn runner(&self, name: &str, peer: Option<IpAddr>) -> Result<Option<Runner>> {
        let Some(pod) = self.pod(name).await? else { return Ok(None) };
        let is_runner = self.runner_selector.iter().all(|(k, v)| {
            pod.pointer(&format!("/metadata/labels/{}", k.replace('/', "~1"))).and_then(Value::as_str)
                == Some(v.as_str())
        });
        let ip = pod.pointer("/status/podIP").and_then(Value::as_str).and_then(|s| s.parse::<IpAddr>().ok());
        let uid = pod.pointer("/metadata/uid").and_then(Value::as_str);
        let (Some(ip), Some(uid), true) = (ip, uid, is_runner) else { return Ok(None) };
        // Pods reach each other without address translation, so the caller's
        // address is its pod's: one runner cannot speak for another.
        if peer.is_some_and(|p| p.to_canonical() != ip.to_canonical()) {
            return Ok(None);
        }
        Ok(Some(Runner { name: name.to_owned(), gateway: ip.to_string(), uid: Some(uid.to_owned()) }))
    }

    async fn dead_runners(&self, names: &[String]) -> Result<Vec<String>> {
        let mut dead = Vec::new();
        for name in names {
            if self.pod(name).await?.is_none_or(|p| pod_is_dead(&p)) {
                dead.push(name.clone());
            }
        }
        Ok(dead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(status: Value) -> Value {
        json!({
            "metadata": {
                "name": "slvc-1",
                "creationTimestamp": "2026-11-09T00:00:00Z",
                "labels": {
                    "selvedge.dev/account": "acct", "selvedge.dev/application": "app",
                    "selvedge.dev/session": "s", "selvedge.dev/runner": "r",
                },
            },
            "status": status,
        })
    }

    fn now() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-11-09T00:01:00Z").unwrap().timestamp()
    }

    #[test]
    fn phases() {
        let i = instance_of(&pod(json!({ "phase": "Pending" })), now()).unwrap();
        assert_eq!((i.phase, i.age_secs, i.runner.as_str()), (Phase::Pending, 60, "r"));

        let running = json!({ "phase": "Running", "podIP": "10.1.2.3",
            "containerStatuses": [{ "name": "app", "state": { "running": {} } }] });
        let i = instance_of(&pod(running), now()).unwrap();
        assert_eq!((i.phase, i.ip.unwrap().to_string().as_str()), (Phase::Running, "10.1.2.3"));

        let exited = json!({ "phase": "Failed", "podIP": "10.1.2.3",
            "containerStatuses": [{ "name": "app", "state": { "terminated": { "exitCode": 3, "reason": "Error" } } }] });
        let i = instance_of(&pod(exited), now()).unwrap();
        assert_eq!((i.phase, i.exit_code), (Phase::Exited, Some(3)));

        let backoff = json!({ "phase": "Pending",
            "containerStatuses": [{ "name": "app", "state": { "waiting": { "reason": "ImagePullBackOff", "message": "denied" } } }] });
        let i = instance_of(&pod(backoff), now()).unwrap();
        assert_eq!((i.phase, i.message.as_deref()), (Phase::Failed, Some("ImagePullBackOff: denied")));

        // The setup step failed: the application never ran.
        let setup = json!({ "phase": "Failed", "message": "init failed" });
        assert_eq!(instance_of(&pod(setup), now()).unwrap().phase, Phase::Failed);

        let unschedulable = json!({ "phase": "Pending",
            "conditions": [{ "type": "PodScheduled", "status": "False", "message": "0/3 nodes" }] });
        let i = instance_of(&pod(unschedulable), now()).unwrap();
        assert_eq!((i.phase, i.message.as_deref()), (Phase::Pending, Some("not scheduled: 0/3 nodes")));
    }

    #[test]
    fn dead_pods() {
        assert!(pod_is_dead(&json!({ "metadata": { "deletionTimestamp": "x" } })));
        assert!(pod_is_dead(&json!({ "status": { "phase": "Failed" } })));
        assert!(!pod_is_dead(&json!({ "metadata": {}, "status": { "phase": "Running" } })));
    }
}
