//! In-memory index of tracked pools.

use std::collections::HashMap;

use alloy::primitives::Address;
use kairos_core::PoolTier;

use crate::{AnyPool, Pool};

/// All pools Kairos is tracking on one chain, keyed by address, with a
/// token-pair index for finding every venue that trades a given pair.
///
/// Pool admission (liquidity, volume and age thresholds) is decided before a
/// pool is registered; the registry only stores and indexes.
#[derive(Debug, Default)]
pub struct PoolRegistry {
    pools: HashMap<Address, Entry>,
    /// Sorted `(token_a, token_b)` -> addresses of pools trading that pair.
    pair_index: HashMap<(Address, Address), Vec<Address>>,
}

#[derive(Debug)]
struct Entry {
    pool: AnyPool,
    tier: PoolTier,
}

fn pair_key(a: Address, b: Address) -> (Address, Address) {
    if a < b { (a, b) } else { (b, a) }
}

/// Every unordered pair of distinct tokens in `tokens`.
fn pairs(tokens: &[Address]) -> impl Iterator<Item = (Address, Address)> + '_ {
    tokens
        .iter()
        .enumerate()
        .flat_map(move |(i, &a)| tokens[i + 1..].iter().map(move |&b| pair_key(a, b)))
}

impl PoolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `pool`, or replaces the pool already registered at its address.
    /// Returns the replaced pool, if any.
    pub fn register(&mut self, pool: impl Into<AnyPool>, tier: PoolTier) -> Option<AnyPool> {
        let pool = pool.into();
        let address = pool.address();
        let replaced = self.remove(&address);
        for key in pairs(pool.tokens()) {
            self.pair_index.entry(key).or_default().push(address);
        }
        self.pools.insert(address, Entry { pool, tier });
        replaced
    }

    /// Removes and returns the pool at `address`.
    pub fn remove(&mut self, address: &Address) -> Option<AnyPool> {
        let entry = self.pools.remove(address)?;
        for key in pairs(entry.pool.tokens()) {
            if let Some(list) = self.pair_index.get_mut(&key) {
                list.retain(|a| a != address);
                if list.is_empty() {
                    self.pair_index.remove(&key);
                }
            }
        }
        Some(entry.pool)
    }

    /// The pool at `address`.
    pub fn get(&self, address: &Address) -> Option<&AnyPool> {
        self.pools.get(address).map(|e| &e.pool)
    }

    /// Mutable access to the pool at `address`, for applying state updates.
    /// A pool's tokens cannot change through this, so the pair index stays
    /// valid.
    pub fn get_mut(&mut self, address: &Address) -> Option<&mut AnyPool> {
        self.pools.get_mut(address).map(|e| &mut e.pool)
    }

    /// Every pool trading `token_a` against `token_b`, in either direction.
    pub fn pools_for_pair(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> impl Iterator<Item = &AnyPool> {
        self.pair_index
            .get(&pair_key(token_a, token_b))
            .into_iter()
            .flatten()
            .filter_map(|a| self.get(a))
    }

    /// Monitoring tier of the pool at `address`.
    pub fn tier(&self, address: &Address) -> Option<PoolTier> {
        self.pools.get(address).map(|e| e.tier)
    }

    /// Changes the monitoring tier. Returns `false` if the pool is unknown.
    pub fn set_tier(&mut self, address: &Address, tier: PoolTier) -> bool {
        match self.pools.get_mut(address) {
            Some(entry) => {
                entry.tier = tier;
                true
            }
            None => false,
        }
    }

    /// Addresses of all pools in `tier`.
    pub fn in_tier(&self, tier: PoolTier) -> impl Iterator<Item = Address> + '_ {
        self.pools
            .iter()
            .filter(move |(_, e)| e.tier == tier)
            .map(|(a, _)| *a)
    }

    /// All registered pools, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &AnyPool> {
        self.pools.values().map(|e| &e.pool)
    }

    /// Number of registered pools.
    pub fn len(&self) -> usize {
        self.pools.len()
    }

    /// Whether no pools are registered.
    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConcentratedLiquidityPool, ConstantProductPool};
    use kairos_core::ProtocolId;
    use kairos_core::addresses::{DAI, USDC, WETH};

    fn v2(byte: u8, a: Address, b: Address) -> ConstantProductPool {
        let (t0, t1) = pair_key(a, b);
        ConstantProductPool::new(Address::repeat_byte(byte), ProtocolId(1), t0, t1, 3000).unwrap()
    }

    #[test]
    fn register_lookup_and_pair_index() {
        let mut reg = PoolRegistry::new();
        reg.register(v2(1, USDC, WETH), PoolTier::Hot);
        reg.register(v2(2, DAI, WETH), PoolTier::Warm);
        let v3 = ConcentratedLiquidityPool::new(
            Address::repeat_byte(3),
            ProtocolId(2),
            USDC,
            WETH,
            500,
            10,
        )
        .unwrap();
        reg.register(v3, PoolTier::Hot);

        assert_eq!(reg.len(), 3);
        assert_eq!(reg.pools_for_pair(WETH, USDC).count(), 2);
        assert_eq!(reg.pools_for_pair(USDC, WETH).count(), 2);
        assert_eq!(reg.pools_for_pair(DAI, WETH).count(), 1);
        assert_eq!(reg.pools_for_pair(DAI, USDC).count(), 0);
        assert_eq!(reg.in_tier(PoolTier::Hot).count(), 2);
    }

    #[test]
    fn re_registering_replaces_without_duplicating_index() {
        let mut reg = PoolRegistry::new();
        assert!(reg.register(v2(1, USDC, WETH), PoolTier::Hot).is_none());
        assert!(reg.register(v2(1, USDC, WETH), PoolTier::Cold).is_some());
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.pools_for_pair(USDC, WETH).count(), 1);
        assert_eq!(reg.tier(&Address::repeat_byte(1)), Some(PoolTier::Cold));
    }

    #[test]
    fn remove_cleans_pair_index() {
        let mut reg = PoolRegistry::new();
        reg.register(v2(1, USDC, WETH), PoolTier::Hot);
        assert!(reg.remove(&Address::repeat_byte(1)).is_some());
        assert!(reg.is_empty());
        assert_eq!(reg.pools_for_pair(USDC, WETH).count(), 0);
        assert!(reg.remove(&Address::repeat_byte(1)).is_none());
    }

    #[test]
    fn set_tier_and_state_updates() {
        let mut reg = PoolRegistry::new();
        let addr = Address::repeat_byte(1);
        reg.register(v2(1, USDC, WETH), PoolTier::Cold);
        assert!(reg.set_tier(&addr, PoolTier::Hot));
        assert!(!reg.set_tier(&Address::ZERO, PoolTier::Hot));

        let Some(AnyPool::ConstantProduct(pool)) = reg.get_mut(&addr) else {
            panic!("expected constant-product pool");
        };
        pool.set_reserves(
            alloy::primitives::U256::from(1),
            alloy::primitives::U256::from(2),
        );
        let Some(AnyPool::ConstantProduct(pool)) = reg.get(&addr) else {
            unreachable!()
        };
        assert_eq!(pool.reserves().1, alloy::primitives::U256::from(2));
    }
}
