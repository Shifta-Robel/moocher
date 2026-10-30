//! Shared types for moocher.
//!
//! * [`config`] — on-disk YAML formats (agents, node, supervisor, providers, routes, workflows)
//! * [`model`]  — domain types (sessions, tokens, todos, tasks, runs)
//! * [`link`]   — node<->supervisor stream payloads
//! * [`api`]    — operator REST bodies
//! * [`archive`] — workspace trees as artifacts (manifests, tar.zst, safe file access)
//! * [`tls`]    — mTLS helpers for the node link
//! * [`proto`]  — generated gRPC transport

pub mod api;
pub mod archive;
pub mod config;
pub mod link;
pub mod model;
pub mod tls;

pub mod proto {
    tonic::include_proto!("moo.link.v1");
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short random id with a prefix, e.g. `s-3f9a1c2b7d`.
pub fn new_id(prefix: &str) -> String {
    use rand::Rng;
    format!("{prefix}-{:010x}", rand::thread_rng().gen::<u64>() & 0xff_ffff_ffff)
}
