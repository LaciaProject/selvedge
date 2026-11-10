//! What an instance is made of: the sidecar's arguments, the setup step and
//! the Kubernetes pod. Pure functions, so the shape of what tenants' images
//! run in is covered by tests.

use serde_json::{Value, json};

use super::InstanceSpec;

/// The port the sidecar's ingress listens on, as workerd expects.
pub const INGRESS_PORT: u16 = 39001;
/// Where workerd would put the sidecar's CA in the container.
pub const CA_DIR: &str = "/etc/cloudflare/certs";
pub const CA_FILE: &str = "cloudflare-containers-ca.crt";
/// The name the sidecar reaches its runner by.
pub const GATEWAY_HOST: &str = "host.docker.internal";

pub const LABEL_COMPONENT: &str = "selvedge.dev/component";
pub const LABEL_ACCOUNT: &str = "selvedge.dev/account";
pub const LABEL_APPLICATION: &str = "selvedge.dev/application";
pub const LABEL_SESSION: &str = "selvedge.dev/session";
pub const LABEL_RUNNER: &str = "selvedge.dev/runner";
pub const COMPONENT: &str = "container";

/// How pods are shaped, from the controller's flags.
#[derive(Clone, Debug)]
pub struct PodSettings {
    /// The sandbox (`kata-qemu`, ...); empty for the cluster's default
    /// runtime, which shares the node's kernel with tenants' containers.
    pub runtime_class: String,
    /// `cloudflare/proxy-everything`, pinned.
    pub sidecar_image: String,
    /// An image with `curl` and `sh`, for the setup step.
    pub setup_image: String,
    /// Resolvers the sidecar forwards the container's DNS queries to.
    pub dns: Vec<String>,
    /// Merged under the pod's `spec` first (node selector, tolerations,
    /// priority class, ...); what the controller sets wins.
    pub template: Value,
    /// Labels every pod of this installation carries.
    pub instance: String,
}

/// The sidecar's arguments, as workerd passes them, with the egress port
/// and network the controller chose.
pub fn sidecar_args(egress_port: u16, gateway_cidr: &str) -> Vec<String> {
    [
        "--http-egress-port",
        &egress_port.to_string(),
        "--http-ingress-address",
        &format!("0.0.0.0:{INGRESS_PORT}"),
        "--docker-gateway-cidr",
        gateway_cidr,
        "--dns-enabled",
        "--tls-intercept",
        "--disable-ipv6",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// The setup step, run once the sidecar listens and before the application
/// starts: applies the egress configuration and saves the sidecar's CA where
/// the application finds it. `$EGRESS` is the configuration; `$1` the
/// directory for the CA.
pub const SETUP_SCRIPT: &str = "set -eu
for i in $(seq 1 100); do
  curl -fsS -o /dev/null -X PUT -H 'content-type: application/json' -d \"$EGRESS\" http://127.0.0.1:39001/egress && break
  [ \"$i\" = 100 ] && exit 1
  sleep 0.1
done
curl -fsS -o \"$1/cloudflare-containers-ca.crt\" http://127.0.0.1:39001/ca";

/// CPU as Kubernetes writes it, in whole millicores, rounded up.
fn millicores(vcpu: f64) -> String {
    format!("{}m", (vcpu * 1000.0).ceil().max(1.0) as u64)
}

/// `KEY=value` as a Kubernetes environment entry. Entries without `=` are
/// skipped, as are names Kubernetes refuses.
fn env_entry(e: &str) -> Option<Value> {
    let (name, value) = e.split_once('=')?;
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| json!({ "name": name, "value": value }))
}

/// A label value: at most 63 characters of `[A-Za-z0-9_.-]`, starting and
/// ending with an alphanumeric. Session and runner names already are.
pub fn label_value(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .take(63)
        .collect();
    cleaned.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_owned()
}

pub fn labels(spec: &InstanceSpec, instance: &str) -> Value {
    json!({
        "app.kubernetes.io/name": "selvedge",
        "app.kubernetes.io/instance": instance,
        "app.kubernetes.io/component": "container",
        LABEL_COMPONENT: COMPONENT,
        LABEL_ACCOUNT: spec.account,
        LABEL_APPLICATION: label_value(&spec.application),
        LABEL_SESSION: label_value(&spec.session),
        LABEL_RUNNER: label_value(&spec.runner.name),
    })
}

/// The name of an instance's image pull secret.
pub fn pull_secret_name(id: &str) -> String {
    format!("{id}-pull")
}

/// The pull secret: credentials for the account's part of the registry only.
/// Owned by the pod once it exists, so it is removed with it.
pub fn pull_secret(spec: &InstanceSpec, instance: &str) -> Value {
    use base64::Engine;
    let (host, user, password) = &spec.pull;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    let config = json!({ "auths": { host: { "username": user, "password": password, "auth": auth } } });
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": pull_secret_name(&spec.id), "labels": labels(spec, instance) },
        "type": "kubernetes.io/dockerconfigjson",
        "stringData": { ".dockerconfigjson": config.to_string() },
    })
}

/// `over` merged into `base`: objects merge key by key, anything else replaces.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                merge(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o,
    }
}

/// The pod of an instance: workerd's container layout, in which the
/// application shares the sidecar's network namespace and all its traffic
/// passes through the sidecar.
///
/// - The sidecar is a native sidecar (an init container that keeps running):
///   it listens before anything else starts and stops after the application.
/// - The setup step then configures egress and saves the CA.
/// - The application never restarts: when it exits, the pod is done, and
///   workerd's `monitor()` reports the exit code.
/// - The pod holds no service account token and learns nothing about the
///   cluster's services or DNS.
pub fn pod(spec: &InstanceSpec, s: &PodSettings) -> Value {
    let r = &spec.resources;
    let limits = json!({
        "cpu": millicores(r.vcpu),
        "memory": format!("{}Mi", r.memory_mib),
        "ephemeral-storage": format!("{}M", r.disk_mb),
    });
    let mut app = json!({
        "name": "app",
        "image": spec.image,
        "imagePullPolicy": "IfNotPresent",
        "env": spec.env.iter().filter_map(|e| env_entry(e)).collect::<Vec<_>>(),
        "resources": { "limits": limits, "requests": limits },
        "volumeMounts": [{ "name": "ca", "mountPath": CA_DIR, "readOnly": true }],
        "securityContext": { "privileged": false },
    });
    if let Some(command) = &spec.command {
        // Docker's `Cmd`: the image's entrypoint stays, its arguments change.
        app["args"] = json!(command);
    }
    let small = json!({
        "limits": { "cpu": "500m", "memory": "128Mi", "ephemeral-storage": "64M" },
        "requests": { "cpu": "10m", "memory": "32Mi", "ephemeral-storage": "64M" },
    });
    let mut pod_spec = s.template.clone();
    if !pod_spec.is_object() {
        pod_spec = json!({});
    }
    let mut enforced = json!({
        "restartPolicy": "Never",
        "automountServiceAccountToken": false,
        "enableServiceLinks": false,
        "hostNetwork": false,
        "hostPID": false,
        "hostIPC": false,
        "shareProcessNamespace": false,
        // Docker applies its default seccomp profile; Kubernetes does not
        // unless the kubelet is told to.
        "securityContext": { "seccompProfile": { "type": "RuntimeDefault" } },
        "terminationGracePeriodSeconds": 10,
        "hostAliases": [{ "ip": spec.runner.gateway, "hostnames": [GATEWAY_HOST] }],
        "dnsPolicy": "None",
        "dnsConfig": { "nameservers": s.dns },
        "imagePullSecrets": [{ "name": pull_secret_name(&spec.id) }],
        "initContainers": [
            {
                "name": "proxy",
                "image": s.sidecar_image,
                // The sidecar leaves traffic to its "gateway network" alone:
                // that is the runner and nothing else. Everything the
                // application sends anywhere else goes through the sidecar.
                "args": sidecar_args(spec.egress_port, &format!("{}/32", spec.runner.gateway)),
                "restartPolicy": "Always",
                "securityContext": { "capabilities": { "add": ["NET_ADMIN"] } },
                "startupProbe": {
                    "tcpSocket": { "port": INGRESS_PORT },
                    "periodSeconds": 1,
                    "failureThreshold": 60,
                },
                "resources": small,
            },
            {
                "name": "setup",
                "image": s.setup_image,
                "command": ["sh", "-c", SETUP_SCRIPT, "setup", "/ca"],
                "env": [{ "name": "EGRESS", "value": spec.egress.to_string() }],
                "volumeMounts": [{ "name": "ca", "mountPath": "/ca" }],
                "securityContext": { "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] } },
                "resources": small,
            },
        ],
        "containers": [app],
        "volumes": [{ "name": "ca", "emptyDir": { "sizeLimit": "1Mi" } }],
    });
    if !s.runtime_class.is_empty() {
        enforced["runtimeClassName"] = json!(s.runtime_class);
    }
    // The template may not smuggle in what the controller decides.
    if let Some(o) = pod_spec.as_object_mut() {
        for k in ["serviceAccountName", "serviceAccount", "ephemeralContainers", "runtimeClassName"] {
            o.remove(k);
        }
    }
    merge(&mut pod_spec, enforced);
    let mut metadata = json!({
        "name": spec.id,
        "labels": labels(spec, &s.instance),
        "annotations": { "selvedge.dev/container-name": spec.name },
    });
    if let Some(uid) = &spec.runner.uid {
        // The runner's pod owns its instances: when it is deleted, Kubernetes
        // removes them.
        metadata["ownerReferences"] = json!([{
            "apiVersion": "v1",
            "kind": "Pod",
            "name": spec.runner.name,
            "uid": uid,
            "controller": false,
            "blockOwnerDeletion": false,
        }]);
    }
    json!({ "apiVersion": "v1", "kind": "Pod", "metadata": metadata, "spec": pod_spec })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::Runner;
    use selvedge_core::containers::Resources;

    fn spec() -> InstanceSpec {
        InstanceSpec {
            id: "slvc-0123".into(),
            account: "0123456789abcdef0123456789abcdef".into(),
            application: "app-id".into(),
            session: "ab12-0123456789ab-g7-1".into(),
            runner: Runner {
                name: "selvedge-runner-0".into(),
                gateway: "10.0.0.7".into(),
                uid: Some("uid-1".into()),
            },
            name: "workerd-x".into(),
            image: "registry.test/0123456789abcdef0123456789abcdef/app@sha256:00".into(),
            resources: Resources { vcpu: 0.0625, memory_mib: 256, disk_mb: 2000 },
            env: vec!["A=1".into(), "B=x=y".into(), "bad".into()],
            command: Some(vec!["serve".into()]),
            egress_port: 8791,
            egress: json!({ "port": 8791 }),
            pull: ("registry.test".into(), "v1".into(), "secret".into()),
        }
    }

    fn settings() -> PodSettings {
        PodSettings {
            runtime_class: "kata-qemu".into(),
            sidecar_image: "proxy:1".into(),
            setup_image: "curl:1".into(),
            dns: vec!["1.1.1.1".into()],
            template: json!({
                "nodeSelector": { "kvm": "true" },
                "serviceAccountName": "admin",
                "hostNetwork": true,
                "automountServiceAccountToken": true,
            }),
            instance: "selvedge".into(),
        }
    }

    #[test]
    fn pod_shape() {
        let p = pod(&spec(), &settings());
        let s = &p["spec"];
        assert_eq!(s["runtimeClassName"], "kata-qemu");
        assert_eq!(s["nodeSelector"]["kvm"], "true");
        // The template cannot loosen what the controller enforces.
        assert_eq!(s["hostNetwork"], false);
        assert_eq!(s["automountServiceAccountToken"], false);
        assert!(s.get("serviceAccountName").is_none());
        assert_eq!(s["restartPolicy"], "Never");
        assert_eq!(s["securityContext"]["seccompProfile"]["type"], "RuntimeDefault");
        assert_eq!(s["hostAliases"][0]["ip"], "10.0.0.7");
        let app = &s["containers"][0];
        assert_eq!(app["args"], json!(["serve"]));
        assert_eq!(app["env"], json!([{ "name": "A", "value": "1" }, { "name": "B", "value": "x=y" }]));
        assert_eq!(
            app["resources"]["limits"],
            json!({ "cpu": "63m", "memory": "256Mi", "ephemeral-storage": "2000M" })
        );
        assert_eq!(s["initContainers"][0]["restartPolicy"], "Always");
        assert_eq!(s["initContainers"][0]["args"][1], "8791");
        assert_eq!(s["initContainers"][0]["args"][5], "10.0.0.7/32");
        assert_eq!(p["metadata"]["ownerReferences"][0]["uid"], "uid-1");
        assert_eq!(p["metadata"]["labels"][LABEL_RUNNER], "selvedge-runner-0");
    }

    #[test]
    fn default_runtime() {
        let mut s = settings();
        s.runtime_class = String::new();
        s.template = Value::Null;
        assert!(pod(&spec(), &s)["spec"].get("runtimeClassName").is_none());
    }

    #[test]
    fn label_values() {
        assert_eq!(label_value("a b/c"), "a_b_c");
        assert_eq!(label_value("-x-"), "x");
        assert_eq!(label_value(&"a".repeat(80)).len(), 63);
    }

    #[test]
    fn pull_secrets() {
        let s = pull_secret(&spec(), "selvedge");
        assert_eq!(s["metadata"]["name"], "slvc-0123-pull");
        let cfg: Value =
            serde_json::from_str(s["stringData"][".dockerconfigjson"].as_str().unwrap()).unwrap();
        assert_eq!(cfg["auths"]["registry.test"]["password"], "secret");
    }
}
