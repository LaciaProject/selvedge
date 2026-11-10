//! Containers: the model shared by the API, the agent and the containers
//! controller. See RFC 0002.

use serde::{Deserialize, Serialize};

/// A Durable Object class with a container application, as an agent needs it
/// to render workerd's `container` option.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerDef {
    pub script_name: String,
    pub class_name: String,
    /// The application's image, pinned by digest when the client sent one.
    pub image: String,
}

/// The id of a Durable Object namespace: stable per account, script and
/// class. The first 32 hex digits of the hash a namespace's `uniqueKey` in
/// workerd's configuration is made of.
pub fn do_namespace_id(account: &str, script: &str, class: &str) -> String {
    do_namespace_digest(account, script, class)[..32].to_owned()
}

/// The hash behind [`do_namespace_id`] and workerd's `uniqueKey`.
pub fn do_namespace_digest(account: &str, script: &str, class: &str) -> String {
    crate::sha256_hex(format!("{account}\0{script}\0{class}").as_bytes())
}

/// What one container instance may use.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    pub vcpu: f64,
    pub memory_mib: u64,
    pub disk_mb: u64,
}

/// Cloudflare's instance types, by name (legacy names included).
pub const INSTANCE_TYPES: &[(&str, Resources)] = &[
    ("lite", Resources { vcpu: 0.0625, memory_mib: 256, disk_mb: 2000 }),
    ("dev", Resources { vcpu: 0.0625, memory_mib: 256, disk_mb: 2000 }),
    ("basic", Resources { vcpu: 0.25, memory_mib: 1024, disk_mb: 4000 }),
    ("standard", Resources { vcpu: 0.5, memory_mib: 4096, disk_mb: 8000 }),
    ("standard-1", Resources { vcpu: 0.5, memory_mib: 4096, disk_mb: 8000 }),
    ("standard-2", Resources { vcpu: 1.0, memory_mib: 6144, disk_mb: 12000 }),
    ("standard-3", Resources { vcpu: 2.0, memory_mib: 8192, disk_mb: 16000 }),
    ("standard-4", Resources { vcpu: 4.0, memory_mib: 12288, disk_mb: 20000 }),
];

pub fn instance_type(name: &str) -> Option<Resources> {
    INSTANCE_TYPES.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
}

/// The resources an application's `configuration` asks for: an instance type,
/// or explicit `vcpu`, `memory_mib` (or `memory`, like `"256MB"`) and
/// `disk.size_mb`; `lite` when it names neither.
pub fn resources_of(configuration: &serde_json::Value) -> Result<Resources, String> {
    if let Some(t) = configuration.get("instance_type").and_then(|v| v.as_str()) {
        return instance_type(t).ok_or_else(|| format!("unknown instance type `{t}`"));
    }
    let lite = instance_type("lite").expect("listed");
    let vcpu = configuration.get("vcpu").and_then(|v| v.as_f64());
    let memory = configuration.get("memory_mib").and_then(|v| v.as_u64());
    let disk = configuration.pointer("/disk/size_mb").and_then(|v| v.as_f64());
    if vcpu.is_none() && memory.is_none() && disk.is_none() {
        return Ok(lite);
    }
    let r = Resources {
        vcpu: vcpu.unwrap_or(lite.vcpu),
        memory_mib: memory.unwrap_or(lite.memory_mib),
        disk_mb: disk.map_or(lite.disk_mb, |d| d.ceil() as u64),
    };
    if !(r.vcpu > 0.0 && r.vcpu <= 64.0) || r.memory_mib == 0 || r.disk_mb == 0 {
        return Err("vcpu, memory_mib and disk.size_mb must be positive".into());
    }
    Ok(r)
}

/// An image reference split into registry host, repository and tag or digest:
/// `registry.example/acct/app@sha256:..` gives
/// `("registry.example", "acct/app", "@sha256:..")`.
pub fn split_image(image: &str) -> Option<(&str, &str, &str)> {
    let (host, rest) = image.split_once('/')?;
    if !(host.contains('.') || host.contains(':') || host == "localhost") {
        return None;
    }
    let at = rest.find('@').or_else(|| rest.rfind(':').filter(|i| !rest[*i..].contains('/')));
    let (repo, reference) = match at {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    (!repo.is_empty()).then_some((host, repo, reference))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resources() {
        assert_eq!(resources_of(&json!({"instance_type": "basic"})).unwrap().memory_mib, 1024);
        assert_eq!(resources_of(&json!({})).unwrap(), instance_type("lite").unwrap());
        let r = resources_of(&json!({"vcpu": 2, "memory_mib": 512, "disk": {"size_mb": 1500.5}})).unwrap();
        assert_eq!(r, Resources { vcpu: 2.0, memory_mib: 512, disk_mb: 1501 });
        assert!(resources_of(&json!({"instance_type": "huge"})).is_err());
        assert!(resources_of(&json!({"vcpu": 0})).is_err());
    }

    #[test]
    fn images() {
        assert_eq!(
            split_image("r.example:5000/abc/app@sha256:00"),
            Some(("r.example:5000", "abc/app", "@sha256:00"))
        );
        assert_eq!(split_image("localhost/abc/app:v1"), Some(("localhost", "abc/app", ":v1")));
        assert_eq!(split_image("r.example/abc/app"), Some(("r.example", "abc/app", "")));
        assert_eq!(split_image("app:v1"), None);
        assert_eq!(split_image("library/app"), None);
    }

    #[test]
    fn namespace_ids() {
        let id = do_namespace_id("a", "s", "C");
        assert_eq!(id.len(), 32);
        assert!(do_namespace_digest("a", "s", "C").starts_with(&id));
        assert_ne!(id, do_namespace_id("a", "s", "D"));
    }
}
