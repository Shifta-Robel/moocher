//! Shared types for opencode-cluster.
//!
//! * [`config`] — on-disk YAML formats (agents, node, supervisor, providers, routes, workflows)
//! * [`model`]  — domain types (sessions, tokens, todos, tasks, runs)
//! * [`link`]   — node<->supervisor stream payloads
//! * [`api`]    — operator REST bodies
//! * [`proto`]  — generated gRPC transport

pub mod api;
pub mod config;
pub mod link;
pub mod model;

pub mod proto {
    tonic::include_proto!("ocl.link.v1");
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short random id with a prefix, e.g. `s-3f9a1c2b7d`.
pub fn new_id(prefix: &str) -> String {
    use rand::Rng;
    format!("{prefix}-{:010x}", rand::thread_rng().gen::<u64>() & 0xff_ffff_ffff)
}
