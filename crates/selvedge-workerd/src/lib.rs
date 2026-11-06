//! Official workerd integration: configuration rendering and process supervision.
//! Selvedge does not patch workerd; everything here uses its public config schema.

pub mod render;
pub mod supervisor;

pub use render::{ClusterOptions, ModuleSource, RenderOptions, Rendered, render, workerd_max_date};
pub use supervisor::WorkerdProcess;
