//! Official workerd integration: configuration rendering and process supervision.
//! Selvedge does not patch workerd; everything here uses its public config schema.

pub mod render;
pub mod supervisor;

const EXPERIMENTAL_FLAGS: &str = include_str!("experimental-flags.txt");

/// Whether a compatibility flag is experimental in the pinned workerd. Tenants
/// may not use these: account processes with Durable Objects run with
/// `--experimental` (cluster mode), which would otherwise enable them, and
/// some (`unsafe_module`, `allow_insecure_inefficient_logged_eval`, ...) would
/// weaken isolation. Regenerate the list with `tools/experimental-flags.py`
/// when bumping workerd.
pub fn is_experimental_flag(flag: &str) -> bool {
    EXPERIMENTAL_FLAGS.lines().any(|l| !l.starts_with('#') && l.trim() == flag)
}

pub use render::{ClusterOptions, ModuleSource, RenderOptions, Rendered, render, workerd_max_date};
pub use supervisor::WorkerdProcess;
