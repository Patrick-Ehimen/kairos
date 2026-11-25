//! Applies decoded logs to pool state and decides when a pool must be
//! reloaded from the chain.
//!
//! This is the synchronous core of [`StateSync`](crate::StateSync): it does
//! no I/O, so every ordering, reorg and gap rule is unit-tested here.
//!
//! # Rules
//!
//! - A pool loaded at block `N` already reflects every log up to and
//!   including block `N`; such logs are dropped.
//! - Logs are applied per pool in `(block, log_index)` order. A log at or
//!   before the pool's last applied position is a duplicate if it was seen
//!   before, and otherwise means a log arrived out of order. The pool is then
//!   reloaded rather than patched.
//! - A `removed` log (reorg) that had already been applied triggers a reload.
//!   Undoing it locally would require the pre-image of the state.
//! - An event that cannot be applied (liquidity underflow, …) means the
//!   local state has drifted, so the pool is reloaded.
//! - A concentrated pool whose price comes within one bitmap word of the
//!   edge of its loaded ticks is reloaded, which recentres the window.
//! - While a pool waits for a reload, its logs are buffered. After the
//!   reload installs state at block `N`, buffered logs after `N` are
//!   replayed.
//! - [`Tracker::audit`] compares the logs received with the canonical logs
//!   from `eth_getLogs`. Any difference (a log lost in a reconnect, or one
//!   from an orphaned block) reloads the affected pool.

use std::collections::{HashMap, HashSet};

use alloy::primitives::{Address, B256};
use kairos_dex::math::tick_math::{MAX_TICK, MIN_TICK};
use kairos_dex::{AnyPool, ConcentratedLiquidityPool, PoolRegistry};

use crate::config::PoolConfig;
use crate::events::{PoolEvent, PoolLog};
use crate::snapshot::StateSnapshot;

/// Identity of a log, including the block it was mined in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LogKey {
    pool: Address,
    block_number: u64,
    block_hash: B256,
    log_index: u64,
}

impl From<&PoolLog> for LogKey {
    fn from(log: &PoolLog) -> Self {
        Self {
            pool: log.pool,
            block_number: log.block_number,
            block_hash: log.block_hash,
            log_index: log.log_index,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// State is current.
    Live,
    /// State is stale; a reload has not been started yet.
    NeedsReload,
    /// A reload is in flight.
    Reloading,
}

#[derive(Debug)]
struct Tracked {
    config: PoolConfig,
    status: Status,
    /// Block the state was last loaded at; logs up to here are included.
    loaded_at: u64,
    /// Position of the last log applied (or `(loaded_at, u64::MAX)`).
    position: (u64, u64),
    /// Logs received while not [`Status::Live`], replayed after the reload.
    buffer: Vec<PoolLog>,
}

/// Pool state plus the bookkeeping needed to keep it exact.
#[derive(Debug)]
pub(crate) struct Tracker {
    registry: PoolRegistry,
    pools: HashMap<Address, Tracked>,
    seq: u64,
    block: u64,
    changed: HashSet<Address>,
    changed_at: HashMap<Address, u64>,
    /// Logs received in blocks after `audited_to`, for [`Self::audit`].
    seen: HashSet<LogKey>,
    audited_to: u64,
}

impl Tracker {
    /// An empty tracker whose first snapshot will have sequence `seq + 1`.
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            registry: PoolRegistry::new(),
            pools: HashMap::new(),
            seq,
            block: 0,
            changed: HashSet::new(),
            changed_at: HashMap::new(),
            seen: HashSet::new(),
            audited_to: 0,
        }
    }

    /// Installs `pool` with state read at `block`, replaying any logs
    /// buffered after that block.
    pub(crate) fn install(&mut self, config: &PoolConfig, pool: AnyPool, block: u64) {
        let address = config.address;
        self.registry.register(pool, config.tier);
        let buffer = match self.pools.get_mut(&address) {
            Some(t) => {
                t.status = Status::Live;
                t.loaded_at = block;
                t.position = (block, u64::MAX);
                std::mem::take(&mut t.buffer)
            }
            None => {
                self.pools.insert(
                    address,
                    Tracked {
                        config: config.clone(),
                        status: Status::Live,
                        loaded_at: block,
                        position: (block, u64::MAX),
                        buffer: Vec::new(),
                    },
                );
                if self.audited_to == 0 {
                    self.audited_to = block;
                }
                Vec::new()
            }
        };
        self.block = self.block.max(block);
        self.changed.insert(address);
        for log in buffer {
            if log.block_number > block {
                self.apply(&log);
            }
        }
    }

    /// Applies one log from the subscription.
    pub(crate) fn apply(&mut self, log: &PoolLog) {
        let key = LogKey::from(log);
        let Some(t) = self.pools.get_mut(&log.pool) else {
            return;
        };

        if log.removed {
            self.seen.remove(&key);
            if t.status != Status::Live {
                t.buffer.push(*log);
            } else if log.position() <= t.position {
                Self::request(t);
            }
            return;
        }

        if t.status != Status::Live {
            self.seen.insert(key);
            t.buffer.push(*log);
            return;
        }

        if log.position() <= t.position {
            let duplicate = log.block_number <= t.loaded_at || !self.seen.insert(key);
            if !duplicate {
                // A log we have not seen, behind one we already applied.
                Self::request(t);
            }
            return;
        }
        self.seen.insert(key);
        t.position = log.position();
        self.block = self.block.max(log.block_number);

        let Some(pool) = self.registry.get_mut(&log.pool) else {
            return;
        };
        let ok = match (pool, log.event) {
            (AnyPool::ConstantProduct(p), PoolEvent::Sync { reserve0, reserve1 }) => {
                p.set_reserves(reserve0, reserve1);
                true
            }
            (
                AnyPool::ConcentratedLiquidity(p),
                PoolEvent::Swap {
                    sqrt_price_x96,
                    liquidity,
                    tick,
                },
            ) => {
                p.set_slot0(sqrt_price_x96, tick, liquidity);
                !near_coverage_edge(p)
            }
            (
                AnyPool::ConcentratedLiquidity(p),
                PoolEvent::ModifyLiquidity {
                    tick_lower,
                    tick_upper,
                    delta,
                },
            ) => p
                .apply_liquidity_delta(tick_lower, tick_upper, delta)
                .is_ok(),
            // An event of the other ABI family: the pool's configured kind is
            // wrong. Reloading would not help, so leave the state as is.
            _ => true,
        };
        self.changed.insert(log.pool);
        if !ok {
            Self::request(t);
        }
    }

    fn request(t: &mut Tracked) {
        if t.status == Status::Live {
            t.status = Status::NeedsReload;
        }
    }

    /// Marks `address` for reload. Unknown pools are ignored.
    pub(crate) fn request_reload(&mut self, address: &Address) {
        if let Some(t) = self.pools.get_mut(address) {
            Self::request(t);
            self.changed.insert(*address);
        }
    }

    /// Pools waiting for a reload that has not started. They are marked as
    /// in flight; pass them back to [`Self::install`] or
    /// [`Self::reload_failed`].
    pub(crate) fn take_reload_requests(&mut self) -> Vec<PoolConfig> {
        let mut out = Vec::new();
        for t in self.pools.values_mut() {
            if t.status == Status::NeedsReload {
                t.status = Status::Reloading;
                out.push(t.config.clone());
            }
        }
        out
    }

    /// Returns pools whose reload failed to the queue.
    pub(crate) fn reload_failed(&mut self, configs: &[PoolConfig]) {
        for c in configs {
            if let Some(t) = self.pools.get_mut(&c.address)
                && t.status == Status::Reloading
            {
                t.status = Status::NeedsReload;
            }
        }
    }

    /// Updates the fee of a concentrated pool. Ignores unknown pools.
    pub(crate) fn set_fee(&mut self, address: &Address, fee_pips: u32) {
        if let Some(AnyPool::ConcentratedLiquidity(p)) = self.registry.get_mut(address)
            && p.fee_pips() != fee_pips
            && p.set_fee_pips(fee_pips).is_ok()
        {
            self.changed.insert(*address);
        }
    }

    /// Configs of tracked pools whose fee must be polled.
    pub(crate) fn dynamic_fee_pools(&self) -> Vec<Address> {
        self.pools
            .values()
            .filter(|t| t.config.kind.fee_is_dynamic())
            .map(|t| t.config.address)
            .collect()
    }

    /// Last block checked by [`Self::audit`].
    pub(crate) fn audited_to(&self) -> u64 {
        self.audited_to
    }

    /// Checks the logs received for blocks `audited_to + 1 ..= to_block`
    /// against `canonical`, the result of `eth_getLogs` over that range, and
    /// marks every pool with a missing or orphaned log for reload.
    pub(crate) fn audit(&mut self, canonical: &[PoolLog], to_block: u64) {
        let from = self.audited_to;
        if to_block <= from {
            return;
        }
        let in_scope = |pools: &HashMap<Address, Tracked>, pool: &Address, block: u64| {
            block > from
                && block <= to_block
                && pools.get(pool).is_some_and(|t| block > t.loaded_at)
        };
        let canon: HashSet<LogKey> = canonical
            .iter()
            .filter(|l| !l.removed && in_scope(&self.pools, &l.pool, l.block_number))
            .map(LogKey::from)
            .collect();

        let mut stale = HashSet::new();
        stale.extend(canon.difference(&self.seen).map(|k| k.pool));
        stale.extend(
            self.seen
                .iter()
                .filter(|k| in_scope(&self.pools, &k.pool, k.block_number) && !canon.contains(k))
                .map(|k| k.pool),
        );
        for pool in stale {
            self.request_reload(&pool);
        }
        self.seen.retain(|k| k.block_number > to_block);
        self.audited_to = to_block;
    }

    /// The next snapshot, if anything changed since the last one.
    pub(crate) fn publish(&mut self) -> Option<StateSnapshot> {
        if self.changed.is_empty() {
            return None;
        }
        self.seq += 1;
        for a in self.changed.drain() {
            self.changed_at.insert(a, self.seq);
        }
        Some(StateSnapshot {
            seq: self.seq,
            block: self.block,
            registry: self.registry.clone(),
            changed_at: self.changed_at.clone(),
            syncing: self
                .pools
                .iter()
                .filter(|(_, t)| t.status != Status::Live)
                .map(|(a, _)| *a)
                .collect(),
        })
    }

    /// Tier of a tracked pool.
    #[cfg(test)]
    fn tier(&self, address: &Address) -> Option<kairos_core::PoolTier> {
        self.registry.tier(address)
    }
}

/// Whether the current tick is within one bitmap word of either end of the
/// loaded tick range, except at the global tick bounds.
fn near_coverage_edge(pool: &ConcentratedLiquidityPool) -> bool {
    let Some(coverage) = pool.tick_coverage() else {
        return true;
    };
    let margin = 256 * pool.tick_spacing();
    let tick = pool.slot0().1;
    (*coverage.start() > MIN_TICK && tick < coverage.start() + margin)
        || (*coverage.end() < MAX_TICK && tick > coverage.end() - margin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PoolKind;
    use alloy::primitives::U256;
    use kairos_core::{PoolTier, ProtocolId};
    use kairos_dex::{ConstantProductPool, TickLiquidity};

    const T0: Address = Address::repeat_byte(0x10);
    const T1: Address = Address::repeat_byte(0x20);
    const V2: Address = Address::repeat_byte(0xa2);
    const V3: Address = Address::repeat_byte(0xa3);
    const LOAD_BLOCK: u64 = 100;

    fn config(address: Address, kind: PoolKind) -> PoolConfig {
        PoolConfig {
            address,
            kind,
            protocol_id: ProtocolId(1),
            fee_ppm: kind.fee_in_config().then_some(3000),
            tier: PoolTier::Hot,
        }
    }

    fn v2_pool(r0: u64) -> AnyPool {
        let mut p = ConstantProductPool::new(V2, ProtocolId(1), T0, T1, 3000).unwrap();
        p.set_reserves(U256::from(r0), U256::from(1000));
        p.into()
    }

    /// Spacing 10, tick 0, ticks loaded over [-25_600, 25_599] with one
    /// position over [-100, 100).
    fn v3_pool() -> AnyPool {
        let mut p = ConcentratedLiquidityPool::new(V3, ProtocolId(1), T0, T1, 500, 10).unwrap();
        p.set_slot0(U256::from(1) << 96, 0, 1_000);
        p.set_ticks(
            [
                (
                    -100,
                    TickLiquidity {
                        gross: 1_000,
                        net: 1_000,
                    },
                ),
                (
                    100,
                    TickLiquidity {
                        gross: 1_000,
                        net: -1_000,
                    },
                ),
            ],
            -25_600..=25_599,
        )
        .unwrap();
        p.into()
    }

    fn tracker() -> Tracker {
        let mut t = Tracker::new(0);
        t.install(&config(V2, PoolKind::UniswapV2), v2_pool(1000), LOAD_BLOCK);
        t.install(&config(V3, PoolKind::UniswapV3), v3_pool(), LOAD_BLOCK);
        t.publish();
        t
    }

    fn sync(block: u64, index: u64, r0: u64) -> PoolLog {
        PoolLog {
            pool: V2,
            block_number: block,
            block_hash: B256::with_last_byte(block as u8),
            log_index: index,
            removed: false,
            event: PoolEvent::Sync {
                reserve0: U256::from(r0),
                reserve1: U256::from(1000),
            },
        }
    }

    fn swap(block: u64, tick: i32) -> PoolLog {
        PoolLog {
            pool: V3,
            block_number: block,
            block_hash: B256::with_last_byte(block as u8),
            log_index: 0,
            removed: false,
            event: PoolEvent::Swap {
                sqrt_price_x96: U256::from(1) << 96,
                liquidity: 1_000,
                tick,
            },
        }
    }

    fn reserve0(t: &Tracker) -> U256 {
        match t.registry.get(&V2) {
            Some(AnyPool::ConstantProduct(p)) => p.reserves().0,
            other => panic!("{other:?}"),
        }
    }

    fn reloads(t: &mut Tracker) -> Vec<Address> {
        let mut v: Vec<_> = t.take_reload_requests().iter().map(|c| c.address).collect();
        v.sort();
        v
    }

    #[test]
    fn applies_in_order_and_drops_logs_covered_by_the_load() {
        let mut t = tracker();
        t.apply(&sync(LOAD_BLOCK, 5, 1)); // already in the loaded state
        assert_eq!(reserve0(&t), U256::from(1000));
        t.apply(&sync(101, 0, 2000));
        t.apply(&sync(101, 3, 3000));
        assert_eq!(reserve0(&t), U256::from(3000));
        t.apply(&sync(101, 3, 3000)); // duplicate
        assert!(reloads(&mut t).is_empty());

        let snap = t.publish().unwrap();
        assert_eq!(snap.block(), 101);
        assert_eq!(snap.changed_since(1).collect::<Vec<_>>(), vec![V2]);
        assert!(t.publish().is_none());
    }

    #[test]
    fn out_of_order_log_triggers_reload() {
        let mut t = tracker();
        t.apply(&sync(102, 0, 2000));
        t.apply(&sync(101, 0, 1500));
        assert_eq!(reloads(&mut t), vec![V2]);
    }

    #[test]
    fn removed_log_triggers_reload_only_if_applied() {
        let mut t = tracker();
        let mut late = sync(105, 0, 1);
        late.removed = true;
        t.apply(&late); // never applied
        assert!(reloads(&mut t).is_empty());

        let applied = sync(103, 0, 2000);
        t.apply(&applied);
        t.apply(&PoolLog {
            removed: true,
            ..applied
        });
        assert_eq!(reloads(&mut t), vec![V2]);
        assert!(t.publish().unwrap().is_syncing(&V2));
    }

    #[test]
    fn reload_buffers_and_replays_newer_logs() {
        let mut t = tracker();
        t.request_reload(&V2);
        let configs = t.take_reload_requests();
        t.apply(&sync(104, 0, 4000)); // covered by the reload below
        t.apply(&sync(106, 0, 6000)); // newer than the reload
        assert!(t.publish().unwrap().pool(&V2).is_none());

        t.install(&configs[0], v2_pool(5000), 105);
        assert_eq!(reserve0(&t), U256::from(6000));
        let snap = t.publish().unwrap();
        assert!(snap.pool(&V2).is_some());
        assert_eq!(t.tier(&V2), Some(PoolTier::Hot));
    }

    #[test]
    fn failed_reload_is_retried() {
        let mut t = tracker();
        t.request_reload(&V2);
        let configs = t.take_reload_requests();
        assert!(t.take_reload_requests().is_empty());
        t.reload_failed(&configs);
        assert_eq!(reloads(&mut t), vec![V2]);
    }

    #[test]
    fn liquidity_underflow_triggers_reload() {
        let mut t = tracker();
        t.apply(&PoolLog {
            pool: V3,
            block_number: 101,
            block_hash: B256::ZERO,
            log_index: 0,
            removed: false,
            event: PoolEvent::ModifyLiquidity {
                tick_lower: -100,
                tick_upper: 100,
                delta: -2_000,
            },
        });
        assert_eq!(reloads(&mut t), vec![V3]);
    }

    #[test]
    fn price_near_tick_coverage_edge_triggers_reload() {
        let mut t = tracker();
        t.apply(&swap(101, 20_000)); // 5_599 ticks from the edge, word is 2_560
        assert!(reloads(&mut t).is_empty());
        t.apply(&swap(102, 23_100)); // 2_499 from the edge
        assert_eq!(reloads(&mut t), vec![V3]);
    }

    #[test]
    fn fee_updates_mark_pool_changed() {
        let mut t = tracker();
        t.set_fee(&V3, 500);
        assert!(t.publish().is_none());
        t.set_fee(&V3, 154);
        let snap = t.publish().unwrap();
        assert_eq!(snap.changed_since(1).collect::<Vec<_>>(), vec![V3]);
        match snap.pool(&V3) {
            Some(AnyPool::ConcentratedLiquidity(p)) => assert_eq!(p.fee_pips(), 154),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn audit_passes_when_logs_match() {
        let mut t = tracker();
        let logs = [sync(101, 0, 1), sync(102, 4, 2)];
        for l in &logs {
            t.apply(l);
        }
        t.audit(&logs, 103);
        assert!(reloads(&mut t).is_empty());
        assert_eq!(t.audited_to(), 103);
        assert!(t.seen.is_empty());
    }

    #[test]
    fn audit_catches_missed_and_orphaned_logs() {
        // Missed: the canonical chain has a log the subscription never sent.
        let mut t = tracker();
        t.apply(&sync(102, 0, 2));
        t.audit(&[sync(101, 0, 1), sync(102, 0, 2)], 102);
        assert_eq!(reloads(&mut t), vec![V2]);

        // Orphaned: a log was applied from a block that is not canonical.
        let mut t = tracker();
        let orphan = sync(101, 0, 1);
        t.apply(&orphan);
        let canonical = PoolLog {
            block_hash: B256::repeat_byte(0xcc),
            ..orphan
        };
        t.audit(&[canonical], 101);
        assert_eq!(reloads(&mut t), vec![V2]);
    }

    #[test]
    fn audit_ignores_logs_covered_by_a_later_load() {
        let mut t = tracker();
        t.request_reload(&V2);
        let configs = t.take_reload_requests();
        t.install(&configs[0], v2_pool(1), 110);
        t.audit(&[sync(105, 0, 1)], 110);
        assert!(reloads(&mut t).is_empty());
    }

    #[test]
    fn snapshot_seq_survives_a_new_tracker() {
        let mut t = Tracker::new(41);
        t.install(&config(V2, PoolKind::UniswapV2), v2_pool(1), 1);
        let snap = t.publish().unwrap();
        assert_eq!(snap.seq(), 42);
        let down = snap.all_syncing(43);
        assert!(down.is_syncing(&V2) && down.pool(&V2).is_none());
    }
}
