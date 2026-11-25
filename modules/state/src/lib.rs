//! Event-driven pool state.
//!
//! Keeps a small, curated set of pools in exact sync with the chain from a
//! filtered log subscription (no `newHeads` or pending-transaction streams),
//! so RPC usage scales with activity on the tracked pools only.
//!
//! - [`config`]: which pools to track, and sync tuning.
//! - [`events`]: decoding of the logs that change pricing state.
//! - [`loader`]: batched reads of pool state at a fixed block.
//! - [`StateSync`]: the task that ties them together and publishes
//!   [`StateSnapshot`]s.

pub mod config;
pub mod events;
pub mod loader;
mod snapshot;
mod sync;
mod tracker;

pub use config::{PoolConfig, PoolKind, StateConfig, SyncSettings};
pub use events::{PoolEvent, PoolLog, TOPICS};
pub use loader::PoolLoader;
pub use snapshot::StateSnapshot;
pub use sync::StateSync;
