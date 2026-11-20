//! Event-driven pool state.
//!
//! Keeps a small, curated set of pools in exact sync with the chain from a
//! filtered log subscription (no `newHeads` or pending-transaction streams),
//! so RPC usage scales with activity on the tracked pools only.
//!
//! - [`config`]: which pools to track.
//! - [`events`]: decoding of the logs that change pricing state.

pub mod config;
pub mod events;

pub use config::{PoolConfig, PoolKind, StateConfig};
pub use events::{PoolEvent, PoolLog, TOPICS};
