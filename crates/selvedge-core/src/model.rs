//! The worker model: what the `/client/v4` upload API accepts, what is stored,
//! and what an agent receives to render a workerd configuration.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{AccountId, NamespaceId, VersionId};

/// The kind of a module, mapped from the multipart part's content type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleKind {
    EsModule,
    CommonJs,
    Text,
    Data,
    Wasm,
    Json,
    Python,
}

impl ModuleKind {
    /// Maps a Cloudflare upload content type to a module kind. `Ok(None)` means the
    /// part is accepted but not a module (source maps).
    pub fn from_content_type(content_type: &str) -> Result<Option<Self>, String> {
        let essence = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        Ok(Some(match essence.as_str() {
            "application/javascript+module" | "text/javascript+module" => Self::EsModule,
            "application/javascript" | "text/javascript" | "application/node" => Self::CommonJs,
            "text/plain" => Self::Text,
            "application/octet-stream" => Self::Data,
            "application/wasm" => Self::Wasm,
            "application/json" => Self::Json,
            "text/x-python" => Self::Python,
            "application/source-map" => return Ok(None),
            other => return Err(format!("unsupported module content type `{other}`")),
        }))
    }
}

/// A module of a script version. Its bytes live in the blob store under `sha256`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Module {
    pub name: String,
    pub kind: ModuleKind,
    pub sha256: String,
}

/// A binding, in the shape Cloudflare's upload metadata uses.
///
/// `secret_text` is stored without its text: secrets live encrypted in their own
/// table and are filled in only when an agent fetches a bundle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Binding {
    PlainText {
        name: String,
        text: String,
    },
    SecretText {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Json {
        name: String,
        json: Value,
    },
    KvNamespace {
        name: String,
        namespace_id: NamespaceId,
    },
    Service {
        name: String,
        service: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entrypoint: Option<String>,
    },
    DurableObjectNamespace {
        name: String,
        class_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script_name: Option<String>,
    },
    D1 {
        name: String,
        id: String,
    },
    Queue {
        name: String,
        queue_name: String,
    },
    R2Bucket {
        name: String,
        bucket_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        jurisdiction: Option<String>,
    },
}

/// Binding types Selvedge knows about but does not implement yet. An upload that
/// uses one is rejected instead of silently missing the binding.
const NOT_YET: &[&str] = &[
    "analytics_engine",
    "ai",
    "browser",
    "hyperdrive",
    "vectorize",
    "mtls_certificate",
    "dispatch_namespace",
    "workflow",
    "version_metadata",
    "send_email",
    "pipelines",
    "secrets_store_secret",
    "images",
    "assets",
];

impl Binding {
    pub fn name(&self) -> &str {
        match self {
            Self::PlainText { name, .. }
            | Self::SecretText { name, .. }
            | Self::Json { name, .. }
            | Self::KvNamespace { name, .. }
            | Self::Service { name, .. }
            | Self::DurableObjectNamespace { name, .. }
            | Self::R2Bucket { name, .. }
            | Self::D1 { name, .. }
            | Self::Queue { name, .. } => name,
        }
    }

    /// Parses one binding from upload metadata with an error that names the
    /// unsupported type, rather than serde's generic one.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let ty = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "binding is missing `type`".to_owned())?;
        if NOT_YET.contains(&ty) {
            return Err(format!("binding type `{ty}` is not supported by Selvedge yet"));
        }
        serde_json::from_value(value.clone()).map_err(|e| format!("invalid `{ty}` binding: {e}"))
    }
}

/// The `metadata` part of a script upload. Unknown fields are ignored.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ScriptMetadata {
    pub main_module: Option<String>,
    pub body_part: Option<String>,
    pub compatibility_date: Option<String>,
    #[serde(default)]
    pub compatibility_flags: Vec<String>,
    #[serde(default)]
    pub bindings: Vec<Value>,
    #[serde(default)]
    pub keep_bindings: Vec<String>,
}

/// Everything an agent needs to run one account: every script's active version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountBundle {
    pub account_id: AccountId,
    /// Increases whenever anything in the bundle changes.
    pub generation: i64,
    pub scripts: Vec<BundleScript>,
    /// Every D1 database of the account, so the REST API can reach one even
    /// when no script binds it.
    #[serde(default)]
    pub d1_databases: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleScript {
    pub name: String,
    pub version_id: VersionId,
    pub main_module: String,
    pub compatibility_date: String,
    pub compatibility_flags: Vec<String>,
    pub modules: Vec<Module>,
    pub bindings: Vec<Binding>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bindings() {
        let b = Binding::parse(&json!({"type": "plain_text", "name": "A", "text": "x"})).unwrap();
        assert_eq!(b.name(), "A");
        let kv = Binding::parse(&json!({
            "type": "kv_namespace", "name": "KV", "namespace_id": "0123456789abcdef0123456789abcdef"
        }))
        .unwrap();
        assert!(matches!(kv, Binding::KvNamespace { .. }));
        let r2 = Binding::parse(&json!({"type": "r2_bucket", "name": "B", "bucket_name": "files"})).unwrap();
        assert!(matches!(r2, Binding::R2Bucket { .. }));
        let d1 = Binding::parse(&json!({"type": "d1", "name": "DB", "id": "x"})).unwrap();
        assert!(matches!(d1, Binding::D1 { .. }));
        let q = Binding::parse(&json!({"type": "queue", "name": "Q", "queue_name": "jobs"})).unwrap();
        assert!(matches!(q, Binding::Queue { .. }));
        let err = Binding::parse(&json!({"type": "vectorize", "name": "V", "index_name": "x"})).unwrap_err();
        assert!(err.contains("not supported"));
        let err =
            Binding::parse(&json!({"type": "kv_namespace", "name": "KV", "namespace_id": "x"})).unwrap_err();
        assert!(err.contains("invalid"));
    }

    #[test]
    fn secret_text_without_text_round_trips() {
        let b = Binding::SecretText { name: "S".into(), text: None };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v, json!({"type": "secret_text", "name": "S"}));
    }

    #[test]
    fn module_kinds() {
        assert_eq!(
            ModuleKind::from_content_type("application/javascript+module").unwrap(),
            Some(ModuleKind::EsModule)
        );
        assert_eq!(ModuleKind::from_content_type("application/source-map").unwrap(), None);
        assert!(ModuleKind::from_content_type("image/png").is_err());
    }
}
