//! Selvedge persistence.
//!
//! PostgreSQL holds all metadata and KV data; module bytes are content-addressed
//! blobs in an S3-compatible bucket. Migrations are append-only.

pub mod assets;
pub mod blobs;
pub mod cron;
pub mod db;
pub mod queues;
pub mod r2;
pub mod repo;
pub mod secrets;

pub use blobs::Blobs;
pub use db::Db;
pub use repo::{
    Account, D1Info, KvEntry, KvKey, KvNamespace, NewVersion, Principal, Route, ScriptInfo, VersionInfo,
};
pub use secrets::SecretBox;
