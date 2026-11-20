//! The list of pools to track, loaded from TOML.
//!
//! The config names only what cannot be read from the chain: which pools to
//! track, which ABI they speak, their executor protocol id and, for V2-style
//! pools, the swap fee (not exposed on chain). Tokens, V3 fees and tick
//! spacings are read from the pool at startup, so they cannot drift from
//! the config.
//!
//! ```toml
//! [[pool]]
//! address = "0x1111111111111111111111111111111111111111"
//! kind = "uniswap_v3"
//! protocol_id = 2
//!
//! [[pool]]
//! address = "0x2222222222222222222222222222222222222222"
//! kind = "uniswap_v2"
//! protocol_id = 1
//! fee_ppm = 3000
//! tier = "warm"
//! ```

use std::collections::HashSet;
use std::path::Path;

use alloy::primitives::Address;
use kairos_core::{FEE_DENOMINATOR, KairosError, PoolTier, ProtocolId, Result};
use serde::Deserialize;

/// Pools to track on one chain.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    /// Tracked pools, from `[[pool]]` tables.
    #[serde(rename = "pool", default)]
    pub pools: Vec<PoolConfig>,
}

/// The ABI a pool exposes, which decides how it is loaded and which events
/// update it. Forks with an identical ABI and math use the same kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    /// `getReserves()` and `Sync`: Uniswap V2 and exact forks.
    UniswapV2,
    /// `slot0()`, `ticks()`, `Swap`, `Mint`, `Burn`: Uniswap V3 and exact
    /// forks (Sushi V3). Not Pancake V3 or Aerodrome Slipstream, whose events
    /// and tick layout differ.
    UniswapV3,
}

/// One tracked pool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    /// Pool contract address.
    pub address: Address,
    /// ABI the pool speaks.
    pub kind: PoolKind,
    /// Executor registry id of the venue.
    pub protocol_id: ProtocolId,
    /// Swap fee in parts per million. Required for `uniswap_v2` (e.g. `3000`
    /// for 0.30%); not allowed for `uniswap_v3`, whose fee is read on chain.
    pub fee_ppm: Option<u32>,
    /// Refresh priority; defaults to `hot`.
    #[serde(default = "default_tier")]
    pub tier: PoolTier,
}

fn default_tier() -> PoolTier {
    PoolTier::Hot
}

impl StateConfig {
    /// Parses and validates a TOML config.
    ///
    /// # Errors
    ///
    /// [`KairosError::ConfigError`] if the TOML is malformed, a pool appears
    /// twice, or a pool's `fee_ppm` is missing, forbidden or out of range.
    pub fn from_toml(text: &str) -> Result<Self> {
        let config: Self =
            toml::from_str(text).map_err(|e| KairosError::ConfigError(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Reads and parses a TOML config file. See [`Self::from_toml`].
    ///
    /// # Errors
    ///
    /// [`KairosError::Io`] if the file cannot be read, otherwise as
    /// [`Self::from_toml`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_toml(&std::fs::read_to_string(path)?)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for pool in &self.pools {
            let err = |msg: &str| {
                Err(KairosError::ConfigError(format!(
                    "pool {}: {msg}",
                    pool.address
                )))
            };
            if !seen.insert(pool.address) {
                return err("listed more than once");
            }
            match (pool.kind, pool.fee_ppm) {
                (PoolKind::UniswapV2, None) => return err("uniswap_v2 pools need fee_ppm"),
                (PoolKind::UniswapV2, Some(fee)) if fee >= FEE_DENOMINATOR => {
                    return err("fee_ppm must be below 1000000");
                }
                (PoolKind::UniswapV3, Some(_)) => {
                    return err("uniswap_v3 fees are read on chain; remove fee_ppm");
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3: &str = "0x1111111111111111111111111111111111111111";
    const V2: &str = "0x2222222222222222222222222222222222222222";

    #[test]
    fn parses_example() {
        let config = StateConfig::from_toml(&format!(
            r#"
            [[pool]]
            address = "{V3}"
            kind = "uniswap_v3"
            protocol_id = 2

            [[pool]]
            address = "{V2}"
            kind = "uniswap_v2"
            protocol_id = 1
            fee_ppm = 3000
            tier = "warm"
            "#
        ))
        .unwrap();
        assert_eq!(config.pools.len(), 2);
        assert_eq!(config.pools[0].kind, PoolKind::UniswapV3);
        assert_eq!(config.pools[0].tier, PoolTier::Hot);
        assert_eq!(config.pools[0].protocol_id, ProtocolId(2));
        assert_eq!(config.pools[1].address, V2.parse::<Address>().unwrap());
        assert_eq!(config.pools[1].fee_ppm, Some(3000));
        assert_eq!(config.pools[1].tier, PoolTier::Warm);
    }

    #[test]
    fn empty_config_is_valid() {
        assert!(StateConfig::from_toml("").unwrap().pools.is_empty());
    }

    fn rejects(body: &str) {
        let text = format!("[[pool]]\naddress = \"{V2}\"\nprotocol_id = 1\n{body}");
        assert!(
            matches!(
                StateConfig::from_toml(&text),
                Err(KairosError::ConfigError(_))
            ),
            "accepted: {body}"
        );
    }

    #[test]
    fn invalid_pools_are_rejected() {
        rejects("kind = \"uniswap_v2\"");
        rejects("kind = \"uniswap_v2\"\nfee_ppm = 1000000");
        rejects("kind = \"uniswap_v3\"\nfee_ppm = 500");
        rejects("kind = \"curve\"");
        rejects("kind = \"uniswap_v3\"\nfee = 500");
        rejects(&format!(
            "kind = \"uniswap_v3\"\n[[pool]]\naddress = \"{V2}\"\nkind = \"uniswap_v3\"\nprotocol_id = 2"
        ));
    }
}
