//! The list of pools to track, loaded from TOML.
//!
//! The config names only what cannot be read from the chain: which pools to
//! track, which ABI they speak, their executor protocol id and, for Uniswap
//! V2-style pools, the swap fee (not exposed on chain). Tokens, fees and tick
//! spacings are read from the pool at startup, so they cannot drift from the
//! config.
//!
//! ```toml
//! [sync]               # optional; these are the defaults
//! tick_window_words = 2
//! fee_refresh_secs = 60
//! audit_secs = 300
//! audit_lag_blocks = 5
//! max_log_range = 500
//!
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
use kairos_core::{AmmKind, FEE_DENOMINATOR, KairosError, PoolTier, ProtocolId, Result};
use serde::Deserialize;

/// Pools to track on one chain, and how to keep them in sync.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    /// Sync tuning, from the optional `[sync]` table.
    #[serde(default)]
    pub sync: SyncSettings,
    /// Tracked pools, from `[[pool]]` tables.
    #[serde(rename = "pool", default)]
    pub pools: Vec<PoolConfig>,
}

/// Tuning for [`StateSync`](crate::StateSync). Each setting trades RPC usage
/// against freshness or coverage.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncSettings {
    /// Tick-bitmap words loaded on each side of a concentrated pool's current
    /// word. One word spans `256 × tick_spacing` ticks. Swaps that would run
    /// past the loaded range are not quoted, and a pool is reloaded when its
    /// price comes within one word of the edge.
    pub tick_window_words: u16,
    /// How often to re-read fees that change without an event (Slipstream's
    /// dynamic fee), in seconds.
    pub fee_refresh_secs: u64,
    /// How often to compare the logs received over the subscription with
    /// `eth_getLogs`, to catch logs lost during a WebSocket reconnect, in
    /// seconds.
    pub audit_secs: u64,
    /// The audit stops this many blocks behind the head, so it does not flag
    /// logs that are still in flight on the subscription.
    pub audit_lag_blocks: u64,
    /// Largest block range per `eth_getLogs` request. Lower it if the RPC
    /// provider caps the range.
    pub max_log_range: u64,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            tick_window_words: 2,
            fee_refresh_secs: 60,
            audit_secs: 300,
            audit_lag_blocks: 5,
            max_log_range: 500,
        }
    }
}

/// The ABI a pool exposes, which decides how it is loaded and which events
/// update it. Forks with an identical ABI and math use the same kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    /// `getReserves()` and `Sync(uint112,uint112)`: Uniswap V2 and exact
    /// forks. The fee is not readable on chain, so `fee_ppm` is required.
    UniswapV2,
    /// Aerodrome (Solidly-style) *volatile* pools: `getReserves()`,
    /// `Sync(uint256,uint256)`, fee from the factory's `getFee`. Stable pools
    /// use different math and are rejected at load.
    AerodromeV2,
    /// `slot0()`, `ticks()`, `Swap`, `Mint`, `Burn`: Uniswap V3 and exact
    /// forks (Sushi V3). Not Pancake V3, whose events differ.
    UniswapV3,
    /// Aerodrome Slipstream: Uniswap V3 events and math, a shorter `slot0()`
    /// and longer `ticks()` (only the shared leading fields are read), and a
    /// fee that can change without an event, so it is re-read every
    /// [`SyncSettings::fee_refresh_secs`].
    Slipstream,
}

impl PoolKind {
    /// Math family the pool is priced with.
    pub fn amm_kind(self) -> AmmKind {
        match self {
            PoolKind::UniswapV2 | PoolKind::AerodromeV2 => AmmKind::ConstantProduct,
            PoolKind::UniswapV3 | PoolKind::Slipstream => AmmKind::ConcentratedLiquidity,
        }
    }

    /// Whether the fee comes from config (`true`) or from the chain.
    pub fn fee_in_config(self) -> bool {
        self == PoolKind::UniswapV2
    }

    /// Whether the fee can change without an event and must be polled.
    pub fn fee_is_dynamic(self) -> bool {
        self == PoolKind::Slipstream
    }

    fn name(self) -> &'static str {
        match self {
            PoolKind::UniswapV2 => "uniswap_v2",
            PoolKind::AerodromeV2 => "aerodrome_v2",
            PoolKind::UniswapV3 => "uniswap_v3",
            PoolKind::Slipstream => "slipstream",
        }
    }
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
    /// for 0.30%); not allowed for other kinds, whose fee is read on chain.
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
            match (pool.kind.fee_in_config(), pool.fee_ppm) {
                (true, None) => return err(&format!("{} pools need fee_ppm", pool.kind.name())),
                (true, Some(fee)) if fee >= FEE_DENOMINATOR => {
                    return err("fee_ppm must be below 1000000");
                }
                (false, Some(_)) => {
                    return err(&format!(
                        "{} fees are read on chain; remove fee_ppm",
                        pool.kind.name()
                    ));
                }
                _ => {}
            }
        }
        let sync = &self.sync;
        if sync.tick_window_words == 0
            || sync.fee_refresh_secs == 0
            || sync.audit_secs == 0
            || sync.max_log_range == 0
        {
            return Err(KairosError::ConfigError(
                "[sync] tick_window_words, fee_refresh_secs, audit_secs and max_log_range must be non-zero".into(),
            ));
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
        let config = StateConfig::from_toml("").unwrap();
        assert!(config.pools.is_empty());
        assert_eq!(config.sync, SyncSettings::default());
    }

    #[test]
    fn aerodrome_kinds_and_sync_settings() {
        let config = StateConfig::from_toml(&format!(
            r#"
            [sync]
            fee_refresh_secs = 30

            [[pool]]
            address = "{V3}"
            kind = "slipstream"
            protocol_id = 4

            [[pool]]
            address = "{V2}"
            kind = "aerodrome_v2"
            protocol_id = 3
            "#
        ))
        .unwrap();
        assert_eq!(config.sync.fee_refresh_secs, 30);
        assert_eq!(config.sync.tick_window_words, 2);
        assert_eq!(
            config.pools[0].kind.amm_kind(),
            AmmKind::ConcentratedLiquidity
        );
        assert!(config.pools[0].kind.fee_is_dynamic());
        assert_eq!(config.pools[1].kind.amm_kind(), AmmKind::ConstantProduct);

        assert!(StateConfig::from_toml("[sync]\nmax_log_range = 0").is_err());
        assert!(StateConfig::from_toml("[sync]\nbogus = 1").is_err());
    }

    /// The pool lists shipped in `config/pools/` must stay loadable.
    #[test]
    fn shipped_pool_lists_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/pools");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "toml") {
                let config =
                    StateConfig::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(!config.pools.is_empty(), "{}", path.display());
            }
        }
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
        rejects("kind = \"aerodrome_v2\"\nfee_ppm = 3000");
        rejects("kind = \"slipstream\"\nfee_ppm = 500");
        rejects(&format!(
            "kind = \"uniswap_v3\"\n[[pool]]\naddress = \"{V2}\"\nkind = \"uniswap_v3\"\nprotocol_id = 2"
        ));
    }
}
