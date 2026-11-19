//! Core domain types shared by every Kairos crate.
//!
//! These types describe the data that moves through the pipeline:
//!
//! ```text
//! pools (AmmKind, ProtocolId, PoolId, Token, PoolTier)
//!   -> detector  (ArbHop, ArbOpportunity)
//!   -> sim       (SimulationResult, SwapStep, ValidatedArb)
//!   -> execution (SystemState, NodeState)
//! ```
//!
//! # How venues are identified
//!
//! Kairos mirrors the on-chain executor (`contracts/src/libraries/KairosTypes.sol`)
//! and identifies a venue with two separate values:
//!
//! - [`AmmKind`] is the *math family* (constant product, concentrated liquidity,
//!   …). It decides which pricing formula `dex` uses and which swap routine
//!   the executor runs. It is a small closed set shared by all chains.
//! - [`ProtocolId`] is the per-chain id of a specific venue (Uniswap V2, Sushi,
//!   Camelot, …) in the executor's on-chain registry. It is open-ended and is
//!   assigned at deploy time, so it is configuration, not code.
//!
//! Supporting a new fork of an existing family therefore needs only a registry
//! entry and config, not a new Rust or Solidity variant.
//!
//! All token amounts are raw on-chain integers (`U256`, scaled by the token's
//! decimals) and all profits / gas costs are denominated in wei.

use alloy::primitives::{Address, Bytes, U256, aliases::U24};
use serde::{Deserialize, Serialize};

use crate::error::{KairosError, Result};

/// Denominator for pool fees expressed in parts-per-million.
///
/// Matches `FEE_DENOMINATOR` in `KairosExecutorBase.sol`: `3_000` is 0.30%.
pub const FEE_DENOMINATOR: u32 = 1_000_000;

/// The AMM math family of a pool.
///
/// Mirrors `enum AmmKind` in `KairosTypes.sol`. The `u8` discriminants **must**
/// equal the Solidity values, because they cross the ABI boundary (e.g. when
/// reading the on-chain registry). Never reorder or reuse them.
///
/// Solidity's `Unset = 0` ("not registered") has no Rust variant. Use
/// `Option<AmmKind>` where a kind may be missing, and note that
/// [`AmmKind::try_from`] rejects `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum AmmKind {
    /// `x * y = k` pairs: Uniswap V2, SushiSwap, Camelot V2, Solidly volatile
    /// pairs. Push-based; the executor encodes the swap itself and reads the fee
    /// from [`SwapStep::fee_ppm`].
    ConstantProduct = 1,
    /// Tick-based concentrated liquidity: Uniswap V3, Algebra (Camelot V3),
    /// Ramses V2, Pancake V3. Swapped via callback, so raw calldata is rejected.
    ConcentratedLiquidity = 2,
    /// Curve StableSwap. Pull-based; requires raw calldata in [`SwapStep::data`].
    StableSwap = 3,
    /// Balancer V2 weighted pools, swapped through the shared Vault. Pull-based;
    /// requires raw calldata in [`SwapStep::data`].
    WeightedPool = 4,
    /// TraderJoe Liquidity Book (bin-based). Push-based; the executor encodes
    /// the swap itself.
    LiquidityBook = 5,
}

impl AmmKind {
    /// Rough gas cost of a single swap in this family.
    ///
    /// Used by the detector to estimate route cost before simulation. These are
    /// heuristics, not exact measurements; the simulator's `gas_used` is the
    /// authoritative figure. Concentrated-liquidity cost grows with ticks
    /// crossed, which is accounted for separately via
    /// [`gas::UNIV3_PER_TICK_GAS`].
    pub fn base_gas(&self) -> u64 {
        match self {
            AmmKind::ConstantProduct => 60_000,
            AmmKind::ConcentratedLiquidity => 180_000,
            AmmKind::StableSwap => 130_000,
            AmmKind::WeightedPool => 120_000,
            // Not yet measured; placeholder until a LB venue is benchmarked.
            AmmKind::LiquidityBook => 120_000,
        }
    }

    /// Whether the executor can build this family's swap call itself.
    ///
    /// When `false` ([`StableSwap`](Self::StableSwap),
    /// [`WeightedPool`](Self::WeightedPool)), the off-chain side must supply raw
    /// calldata in [`SwapStep::data`] or the executor reverts with
    /// `EncodingRequired`.
    pub fn executor_encodes(&self) -> bool {
        matches!(
            self,
            AmmKind::ConstantProduct | AmmKind::ConcentratedLiquidity | AmmKind::LiquidityBook
        )
    }
}

impl TryFrom<u8> for AmmKind {
    /// The unrecognised value, returned unchanged.
    type Error = u8;

    /// Converts a Solidity `AmmKind` value. `0` (`Unset`) and any unknown
    /// value are rejected.
    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            1 => Ok(AmmKind::ConstantProduct),
            2 => Ok(AmmKind::ConcentratedLiquidity),
            3 => Ok(AmmKind::StableSwap),
            4 => Ok(AmmKind::WeightedPool),
            5 => Ok(AmmKind::LiquidityBook),
            other => Err(other),
        }
    }
}

/// A venue's id in the executor's on-chain protocol registry.
///
/// Mirrors `SwapStep.protocolId` (`uint16`) in `KairosTypes.sol`. Ids are
/// per-chain and assigned when the registry is populated, so the same venue
/// may have different ids on different chains. Load them from configuration;
/// do not hard-code them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolId(pub u16);

/// Returns the decimals of a well-known mainnet token without an RPC call.
///
/// Covers the tokens in [`addresses`]. Returns `None` for anything else, in
/// which case the caller must fetch `decimals()` from the token contract.
pub fn known_token_decimals(addr: &Address) -> Option<u8> {
    use addresses::{DAI, USDC, USDT, WBTC, WETH};
    match *addr {
        WETH => Some(18),
        USDC => Some(6),
        USDT => Some(6),
        DAI => Some(18),
        WBTC => Some(8),
        _ => None,
    }
}

/// Unique identifier for a pool on one chain.
///
/// Carries both the math family and the registry id so that a pool is never
/// priced with the wrong formula, and never submitted under the wrong venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId {
    /// On-chain address of the pool / pair contract.
    pub address: Address,
    /// Math family used to price this pool.
    pub kind: AmmKind,
    /// Venue id in the executor's on-chain registry.
    pub protocol_id: ProtocolId,
}

/// An ERC-20 token with the metadata needed for pricing and display.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Token {
    /// Token contract address.
    pub address: Address,
    /// Number of decimals the raw amounts are scaled by (e.g. 6 for USDC).
    pub decimals: u8,
    /// Ticker symbol, for logs and display only. Not unique and not trusted.
    pub symbol: String,
}

/// How often a pool's state is refreshed.
///
/// Pools are tiered by activity / TVL so that refresh budget is spent on the
/// pools most likely to be part of a profitable route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolTier {
    /// Refreshed every block.
    Hot,
    /// Refreshed every 5 blocks.
    Warm,
    /// Refreshed every 20 blocks.
    Cold,
}

/// One hop of an arbitrage route as seen by the detector.
///
/// The output of hop `i` is the input of hop `i + 1`, and the last hop's
/// `token_out` equals the first hop's `token_in` (a closed cycle).
#[derive(Debug, Clone)]
pub struct ArbHop {
    /// Math family of the pool being swapped through.
    pub kind: AmmKind,
    /// Venue id in the executor's on-chain registry.
    pub protocol_id: ProtocolId,
    /// Pool / pair address.
    pub pool_address: Address,
    /// Token sold into the pool.
    pub token_in: Address,
    /// Token received from the pool.
    pub token_out: Address,
    /// Raw input amount.
    pub amount_in: U256,
    /// Raw output amount predicted from cached pool state.
    pub expected_out: U256,
    /// Estimated gas for this hop (see [`AmmKind::base_gas`]).
    pub estimated_gas: u64,
}

/// One hop of a route in the exact shape the on-chain executor accepts.
///
/// Mirrors `struct SwapStep` in `KairosTypes.sol` field-for-field and
/// type-for-type, so it can be ABI-encoded without any reshaping.
///
/// There is deliberately no `token_in` or `amount_in`. The executor derives
/// both on-chain: hop `i`'s input token is hop `i - 1`'s `token_out` (or the
/// flash-loaned asset for hop 0), and its input amount is the balance hop
/// `i - 1` actually produced. The off-chain view of those values lives on
/// [`ArbHop`].
///
/// Call [`SwapStep::validate`] before submission to catch steps the executor
/// would revert on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapStep {
    /// Venue id; must be registered and enabled in the executor's registry.
    /// (`protocolId`, `uint16`)
    pub protocol_id: ProtocolId,
    /// Pool fee in parts-per-million of [`FEE_DENOMINATOR`] (`3_000` = 0.30%).
    /// Read only for [`AmmKind::ConstantProduct`] and must be `0` for every
    /// other family. (`feeP`, `uint24`)
    pub fee_ppm: U24,
    /// Pool / pair address. (`pool`, `address`)
    pub pool: Address,
    /// Token this hop produces. (`tokenOut`, `address`)
    pub token_out: Address,
    /// Hard slippage floor. The executor measures its actual balance increase
    /// and reverts with `InsufficientOutput` if it is below this.
    /// (`minAmountOut`, `uint256`)
    pub min_amount_out: U256,
    /// Escape-hatch calldata. Empty means the executor encodes the call
    /// itself. Required for [`AmmKind::StableSwap`] and
    /// [`AmmKind::WeightedPool`]; rejected for
    /// [`AmmKind::ConcentratedLiquidity`]. (`data`, `bytes`)
    pub data: Bytes,
}

impl SwapStep {
    /// Checks this step against the rules the executor enforces for `kind`, so
    /// that a bad step is rejected off-chain instead of reverting on-chain.
    ///
    /// `kind` must be the registry kind of [`Self::protocol_id`]; this function
    /// cannot check that the id is actually registered or enabled.
    ///
    /// # Errors
    ///
    /// Returns [`KairosError::InvalidSwapStep`] if:
    /// - `fee_ppm >= FEE_DENOMINATOR` for a constant-product step (on-chain:
    ///   `InvalidFee`);
    /// - `fee_ppm != 0` for any other family (ignored on-chain, so a non-zero
    ///   value means the caller confused fields);
    /// - `data` is non-empty for concentrated liquidity (on-chain:
    ///   `RawCalldataUnsupported`);
    /// - `data` is empty for a family the executor cannot encode (on-chain:
    ///   `EncodingRequired`).
    pub fn validate(&self, kind: AmmKind) -> Result<()> {
        let fee = self.fee_ppm.to::<u32>();
        let invalid = |reason: String| {
            Err(KairosError::InvalidSwapStep {
                pool: self.pool,
                reason,
            })
        };

        if kind == AmmKind::ConstantProduct {
            if fee >= FEE_DENOMINATOR {
                return invalid(format!("fee {fee} ppm must be below {FEE_DENOMINATOR}"));
            }
        } else if fee != 0 {
            return invalid(format!("fee must be 0 for {kind:?}, got {fee} ppm"));
        }

        if kind == AmmKind::ConcentratedLiquidity && !self.data.is_empty() {
            return invalid("raw calldata is not supported for ConcentratedLiquidity".into());
        }
        if !kind.executor_encodes() && self.data.is_empty() {
            return invalid(format!("{kind:?} requires raw calldata in `data`"));
        }

        Ok(())
    }
}

/// A candidate arbitrage found by the detector, before EVM simulation.
///
/// Profit figures come from off-chain math on cached state and may be stale or
/// optimistic. Only a [`ValidatedArb`] should be executed.
#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    /// Unique identifier, used to correlate logs and metrics across stages.
    pub id: String,
    /// Ordered hops forming a closed cycle.
    pub hops: Vec<ArbHop>,
    /// Gross profit (final output minus initial input), in wei.
    pub total_profit_wei: U256,
    /// Estimated total gas for the whole route.
    pub total_gas: u64,
    /// `total_gas` priced at the current gas price, in wei.
    pub gas_cost_wei: U256,
    /// `total_profit_wei - gas_cost_wei`, in wei.
    pub net_profit_wei: U256,
    /// Block whose state the opportunity was computed against.
    pub block_number: u64,
    /// Detection time, in nanoseconds since the Unix epoch.
    pub timestamp_ns: i64,
}

/// An opportunity that passed EVM simulation and is ready to execute.
///
/// Profit and gas figures here come from the simulator, not from the
/// detector's estimates.
#[derive(Debug, Clone)]
pub struct ValidatedArb {
    /// Same identifier as the originating [`ArbOpportunity`].
    pub id: String,
    /// Hops of the route, as detected.
    pub hops: Vec<ArbHop>,
    /// Executable form of `hops`, with slippage floors applied.
    pub steps: Vec<SwapStep>,
    /// Simulated gross profit, in wei.
    pub total_profit_wei: U256,
    /// Simulated gas used by the whole transaction.
    pub total_gas: u64,
    /// `total_gas` priced at the target gas price, in wei.
    pub gas_cost_wei: U256,
    /// `total_profit_wei - gas_cost_wei`, in wei.
    pub net_profit_wei: U256,
    /// Block the simulation forked from.
    pub block_number: u64,
    /// Validation time, in nanoseconds since the Unix epoch.
    pub timestamp_ns: i64,
    /// Asset borrowed via flash loan to fund the first hop.
    pub flashloan_token: Address,
    /// Raw amount of `flashloan_token` to borrow.
    pub flashloan_amount: U256,
    /// Fully encoded call to the executor contract.
    pub calldata: Vec<u8>,
}

/// Outcome of simulating a route in the EVM.
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// `true` if the transaction executed without reverting.
    pub success: bool,
    /// Profit realised in the simulation, in wei. Zero on revert.
    pub profit_wei: U256,
    /// Gas consumed by the simulated transaction.
    pub gas_used: u64,
    /// Decoded revert reason when `success` is `false`.
    pub revert_reason: Option<String>,
}

/// Global operating mode, controlled by the risk manager.
///
/// Ordered from least to most restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemState {
    /// Normal operation: detecting and executing.
    Running,
    /// Executing under reduced limits (e.g. after repeated reverts or node
    /// trouble).
    Degraded,
    /// Execution suspended; detection keeps running. Resumes automatically
    /// once the triggering condition clears.
    Paused,
    /// Execution stopped until an operator intervenes.
    Halted,
}

/// Health of a single node connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    /// Connection established but not yet health-checked.
    Connected,
    /// Responding promptly and in sync with the chain head.
    Healthy,
    /// Reachable but lagging or slow; used only if no healthy node exists.
    Degraded,
    /// Connection lost; reconnect in progress.
    Reconnecting,
    /// Reconnect attempts exhausted; node is out of rotation.
    Failed,
}

/// Well-known Ethereum mainnet addresses.
///
/// These are mainnet-only. Other chains need their own configuration.
pub mod addresses {
    use alloy::primitives::{Address, address};

    /// Wrapped Ether (18 decimals).
    pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    /// USD Coin (6 decimals).
    pub const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    /// Tether USD (6 decimals).
    pub const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    /// Dai Stablecoin (18 decimals).
    pub const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    /// Wrapped Bitcoin (8 decimals).
    pub const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

    /// Aave V3 Pool, the flash-loan provider.
    pub const AAVE_V3_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");

    /// Uniswap V2 factory, used to enumerate and verify V2 pairs.
    pub const UNISWAP_V2_FACTORY: Address = address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");

    /// Uniswap V3 factory, used to enumerate and verify V3 pools.
    pub const UNISWAP_V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");

    /// SushiSwap factory, used to enumerate and verify Sushi pairs.
    pub const SUSHISWAP_FACTORY: Address = address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");
}

/// Gas constants used to estimate route cost before simulation.
///
/// A route's estimate is roughly
/// `TX_BASE_GAS + FLASHLOAN_BASE_GAS + EXECUTOR_OVERHEAD_GAS + Σ hop gas`.
pub mod gas {
    /// Overhead of the Aave V3 flash loan (borrow, callback and repay).
    pub const FLASHLOAN_BASE_GAS: u64 = 80_000;
    /// Intrinsic cost of any Ethereum transaction.
    pub const TX_BASE_GAS: u64 = 21_000;
    /// Executor contract overhead outside the swaps themselves (dispatch,
    /// balance checks, profit transfer).
    pub const EXECUTOR_OVERHEAD_GAS: u64 = 30_000;
    /// Extra gas per initialized tick crossed in a Uniswap V3 swap.
    pub const UNIV3_PER_TICK_GAS: u64 = 5_000;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(fee_ppm: u32, data: &'static [u8]) -> SwapStep {
        SwapStep {
            protocol_id: ProtocolId(1),
            fee_ppm: U24::from(fee_ppm),
            pool: Address::ZERO,
            token_out: addresses::WETH,
            min_amount_out: U256::from(1),
            data: Bytes::from_static(data),
        }
    }

    /// Discriminants cross the ABI boundary and must equal `KairosTypes.sol`.
    #[test]
    fn amm_kind_discriminants_match_solidity() {
        assert_eq!(AmmKind::ConstantProduct as u8, 1);
        assert_eq!(AmmKind::ConcentratedLiquidity as u8, 2);
        assert_eq!(AmmKind::StableSwap as u8, 3);
        assert_eq!(AmmKind::WeightedPool as u8, 4);
        assert_eq!(AmmKind::LiquidityBook as u8, 5);
    }

    #[test]
    fn amm_kind_try_from_round_trips_and_rejects_unset() {
        for v in 1..=5u8 {
            assert_eq!(AmmKind::try_from(v).unwrap() as u8, v);
        }
        assert_eq!(AmmKind::try_from(0), Err(0));
        assert_eq!(AmmKind::try_from(6), Err(6));
    }

    #[test]
    fn constant_product_fee_bounds() {
        assert!(step(3_000, b"").validate(AmmKind::ConstantProduct).is_ok());
        assert!(
            step(FEE_DENOMINATOR - 1, b"")
                .validate(AmmKind::ConstantProduct)
                .is_ok()
        );
        assert!(
            step(FEE_DENOMINATOR, b"")
                .validate(AmmKind::ConstantProduct)
                .is_err()
        );
    }

    #[test]
    fn non_constant_product_requires_zero_fee() {
        assert!(
            step(3_000, b"")
                .validate(AmmKind::ConcentratedLiquidity)
                .is_err()
        );
        assert!(
            step(0, b"")
                .validate(AmmKind::ConcentratedLiquidity)
                .is_ok()
        );
    }

    #[test]
    fn concentrated_liquidity_rejects_raw_calldata() {
        let err = step(0, b"\x01").validate(AmmKind::ConcentratedLiquidity);
        assert!(matches!(err, Err(KairosError::InvalidSwapStep { .. })));
    }

    #[test]
    fn pull_based_kinds_require_raw_calldata() {
        for kind in [AmmKind::StableSwap, AmmKind::WeightedPool] {
            assert!(step(0, b"").validate(kind).is_err());
            assert!(step(0, b"\x01").validate(kind).is_ok());
        }
    }

    #[test]
    fn push_based_kinds_accept_raw_calldata() {
        assert!(
            step(3_000, b"\x01")
                .validate(AmmKind::ConstantProduct)
                .is_ok()
        );
        assert!(step(0, b"\x01").validate(AmmKind::LiquidityBook).is_ok());
    }
}
