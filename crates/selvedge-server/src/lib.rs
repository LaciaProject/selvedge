//! Selvedge's server roles. Each runs as a subcommand of the `selvedge` binary:
//!
//! - [`api`]: the `/client/v4` control-plane API and internal endpoints;
//! - [`data`]: the binding data service (KV);
//! - [`agent`]: the runner agent, one workerd process per account;
//! - [`gateway`]: hostname routing to runners.

pub mod agent;
pub mod api;
pub mod data;
pub mod dispatch;
pub mod error;
pub mod gateway;
pub mod kvlogic;
pub mod proxy;
pub mod queues;
pub mod r2;
pub mod r2s3;
pub mod sigv4;
pub mod tail;
pub mod tls;

/// Compares secrets without leaking where they differ through timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
