//! Per-chain settings, loaded from a chain profile TOML.
//!
//! Everything that differs between chains lives here rather than in code, so
//! the detector, AMM math and state modules stay chain-free.
//!
//! ```toml
//! name = "base"
//! chain_id = 8453
//! multicall3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
//! wrapped_native = "0x4200000000000000000000000000000000000006"
//!
//! [data_fee]
//! kind = "op_stack"
//! gas_price_oracle = "0x420000000000000000000000000000000000000F"
//! l1_block = "0x4200000000000000000000000000000000000015"
//!
//! [gas]
//! tx_base = 21000
//! flashloan = 80000
//! executor_overhead = 30000
//! per_initialized_tick = 5000
//! constant_product = 60000
//! concentrated_liquidity = 180000
//! ```

use std::path::Path;

use alloy::primitives::Address;
use serde::Deserialize;

use crate::error::{KairosError, Result};
use crate::fees::DataFeeConfig;
use crate::types::AmmKind;

/// Settings for one chain.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainProfile {
    /// Short name, for logs and metric labels (`"base"`).
    pub name: String,
    /// EIP-155 chain id. Checked against the node at startup so a profile is
    /// never run against the wrong chain.
    pub chain_id: u64,
    /// Multicall3 deployment, used to batch state reads into one `eth_call`.
    pub multicall3: Address,
    /// Wrapped native token (WETH). Profits are valued in it, since gas is
    /// paid in the native token.
    pub wrapped_native: Address,
    /// Where rollup data-fee parameters are read from. `None` for chains
    /// that charge execution gas only.
    #[serde(default)]
    pub data_fee: Option<DataFeeConfig>,
    /// Execution-gas estimates used to rank routes before simulation.
    pub gas: GasEstimates,
}

/// Execution-gas estimates for pricing a route before it is simulated.
///
/// A route's estimate is
/// `tx_base + flashloan + executor_overhead + Σ hop(kind) + per_initialized_tick × ticks crossed`.
///
/// These are heuristics: simulation reports the real gas used. They cover
/// execution gas only; rollup data fees are priced by the chain's
/// [`GasModel`](crate::GasModel) from the profile's `data_fee` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GasEstimates {
    /// Intrinsic cost of any transaction.
    pub tx_base: u64,
    /// Overhead of the flash loan (borrow, callback and repay).
    pub flashloan: u64,
    /// Executor overhead outside the swaps (dispatch, balance checks, profit
    /// transfer).
    pub executor_overhead: u64,
    /// Extra gas per initialized tick crossed in a concentrated-liquidity swap.
    pub per_initialized_tick: u64,
    /// One constant-product swap.
    pub constant_product: Option<u64>,
    /// One concentrated-liquidity swap, excluding ticks crossed.
    pub concentrated_liquidity: Option<u64>,
    /// One StableSwap swap.
    pub stable_swap: Option<u64>,
    /// One weighted-pool swap.
    pub weighted_pool: Option<u64>,
    /// One Liquidity Book swap.
    pub liquidity_book: Option<u64>,
}

impl GasEstimates {
    /// Estimated gas of one swap in a pool of `kind`, or `None` if the profile
    /// has no estimate for that family (it is not traded on this chain).
    pub fn hop(&self, kind: AmmKind) -> Option<u64> {
        match kind {
            AmmKind::ConstantProduct => self.constant_product,
            AmmKind::ConcentratedLiquidity => self.concentrated_liquidity,
            AmmKind::StableSwap => self.stable_swap,
            AmmKind::WeightedPool => self.weighted_pool,
            AmmKind::LiquidityBook => self.liquidity_book,
        }
    }

    /// Fixed per-transaction cost: `tx_base + flashloan + executor_overhead`.
    pub fn route_overhead(&self) -> u64 {
        self.tx_base + self.flashloan + self.executor_overhead
    }
}

impl ChainProfile {
    /// Parses and validates a chain profile.
    ///
    /// # Errors
    ///
    /// [`KairosError::ConfigError`] if the TOML is malformed, `name` is empty
    /// or `chain_id` is zero.
    pub fn from_toml(text: &str) -> Result<Self> {
        let profile: Self =
            toml::from_str(text).map_err(|e| KairosError::ConfigError(e.to_string()))?;
        if profile.name.trim().is_empty() {
            return Err(KairosError::ConfigError(
                "chain profile name is empty".into(),
            ));
        }
        if profile.chain_id == 0 {
            return Err(KairosError::ConfigError(format!(
                "chain profile {}: chain_id must be non-zero",
                profile.name
            )));
        }
        Ok(profile)
    }

    /// Reads and parses a chain profile file. See [`Self::from_toml`].
    ///
    /// # Errors
    ///
    /// [`KairosError::Io`] if the file cannot be read, otherwise as
    /// [`Self::from_toml`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_toml(&std::fs::read_to_string(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"
        name = "test"
        chain_id = 1337
        multicall3 = "0x00000000000000000000000000000000000000ca"
        wrapped_native = "0x00000000000000000000000000000000000000ee"

        [gas]
        tx_base = 21000
        flashloan = 80000
        executor_overhead = 30000
        per_initialized_tick = 5000
        constant_product = 60000
        concentrated_liquidity = 180000
    "#;

    #[test]
    fn parses_profile() {
        let p = ChainProfile::from_toml(PROFILE).unwrap();
        assert_eq!(p.chain_id, 1337);
        assert_eq!(p.gas.hop(AmmKind::ConstantProduct), Some(60_000));
        assert_eq!(p.gas.hop(AmmKind::StableSwap), None);
        assert_eq!(p.gas.route_overhead(), 131_000);
        assert_eq!(p.data_fee, None);
    }

    #[test]
    fn parses_op_stack_data_fee() {
        let text = PROFILE.replace(
            "[gas]",
            "[data_fee]\nkind = \"op_stack\"\n\
             gas_price_oracle = \"0x000000000000000000000000000000000000000f\"\n\
             l1_block = \"0x0000000000000000000000000000000000000015\"\n\n[gas]",
        );
        let p = ChainProfile::from_toml(&text).unwrap();
        assert_eq!(
            p.data_fee,
            Some(DataFeeConfig::OpStack {
                gas_price_oracle: Address::with_last_byte(0x0f),
                l1_block: Address::with_last_byte(0x15),
            })
        );
        for bad in [
            text.replace("op_stack", "arbitrum"),
            text.replace("l1_block", "l1_blocks"),
        ] {
            assert!(ChainProfile::from_toml(&bad).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn rejects_invalid_profiles() {
        for bad in [
            PROFILE.replace("chain_id = 1337", "chain_id = 0"),
            PROFILE.replace("name = \"test\"", "name = \" \""),
            PROFILE.replace("tx_base = 21000", ""),
            format!("{PROFILE}\nunknown = 1"),
        ] {
            assert!(
                matches!(
                    ChainProfile::from_toml(&bad),
                    Err(KairosError::ConfigError(_))
                ),
                "accepted: {bad}"
            );
        }
    }

    /// The profiles shipped in `config/chains/` must stay loadable.
    #[test]
    fn shipped_profiles_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/chains");
        let mut count = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "toml") {
                ChainProfile::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                count += 1;
            }
        }
        assert!(count > 0);
    }
}
