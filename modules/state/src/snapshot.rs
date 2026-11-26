//! Read-only views of pool state, published after every batch of updates.

use std::collections::{HashMap, HashSet};

use alloy::primitives::Address;
use kairos_dex::{AnyPool, PoolRegistry};

/// Pool state as of one point in the log stream.
///
/// Snapshots are immutable and shared behind an `Arc`, so readers never
/// block the sync task. Each one has a sequence number that increases by one
/// per publish; a reader that remembers the last `seq` it processed can ask
/// for exactly the pools that changed since, even if it skipped snapshots.
#[derive(Debug, Clone, Default)]
pub struct StateSnapshot {
    pub(crate) seq: u64,
    pub(crate) block: u64,
    pub(crate) registry: PoolRegistry,
    pub(crate) changed_at: HashMap<Address, u64>,
    pub(crate) syncing: HashSet<Address>,
}

impl StateSnapshot {
    /// Sequence number of this snapshot; `0` before the first load.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Highest block whose logs (or state reads) are reflected.
    pub fn block(&self) -> u64 {
        self.block
    }

    /// All tracked pools, including ones that are being resynced.
    pub fn registry(&self) -> &PoolRegistry {
        &self.registry
    }

    /// The pool at `address`, if it is tracked and its state is current.
    /// Pools waiting for a reload return `None`: their state is stale.
    pub fn pool(&self, address: &Address) -> Option<&AnyPool> {
        if self.syncing.contains(address) {
            return None;
        }
        self.registry.get(address)
    }

    /// Whether the pool at `address` is waiting for a reload. Do not price
    /// routes through it.
    pub fn is_syncing(&self, address: &Address) -> bool {
        self.syncing.contains(address)
    }

    /// Pools whose state changed after snapshot `seq`. Pass `0` for all pools.
    pub fn changed_since(&self, seq: u64) -> impl Iterator<Item = Address> + '_ {
        self.changed_at
            .iter()
            .filter(move |(_, s)| **s > seq)
            .map(|(a, _)| *a)
    }

    /// Copy of this snapshot with every pool marked as syncing, published
    /// while the connection to the node is being re-established.
    pub(crate) fn all_syncing(&self, seq: u64) -> Self {
        Self {
            seq,
            syncing: self
                .registry
                .iter()
                .map(kairos_dex::Pool::address)
                .collect(),
            ..self.clone()
        }
    }
}
