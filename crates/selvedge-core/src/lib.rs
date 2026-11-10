//! Shared types for Selvedge.
//!
//! Everything here is plain data: identifiers, the worker and binding model that
//! the API accepts and the agent renders, and the `/client/v4` response envelope.

pub mod assets;
pub mod containers;
pub mod envelope;
pub mod ids;
pub mod model;
pub mod routes;

pub use containers::ContainerDef;
pub use ids::{AccountId, HexId, NamespaceId, VersionId, new_version_id, validate_script_name};
pub use model::{
    AccountBundle, Binding, BundleScript, Canary, Module, ModuleKind, ScriptMetadata, WorkflowDef,
};

/// Content hash of a blob, as lowercase hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}
