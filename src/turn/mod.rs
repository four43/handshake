//! The built-in TURN server (docs/specs/builtin-turn.md): STUN Binding plus TURN over UDP and TCP, relaying UDP
//! for browsers whose players cannot connect directly. TLS (`turns:`) is the reverse proxy's job.

pub mod allocation;
pub mod auth;
pub mod proxy;
pub mod stun;
mod server;

pub use server::{TurnServer, Tuning};

use std::sync::atomic::AtomicU64;

/// Counters for `/metrics`, shared by the HTTP side and the TURN server.
#[derive(Default)]
pub struct Stats {
    pub allocations: AtomicU64,
    pub allocations_total: AtomicU64,
    /// Peer → client bytes.
    pub bytes_in: AtomicU64,
    /// Client → peer bytes.
    pub bytes_out: AtomicU64,
    pub auth_failures: AtomicU64,
    pub quota_rejections: AtomicU64,
}
