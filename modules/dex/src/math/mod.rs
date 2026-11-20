//! Integer math ported from Uniswap V3 core.
//!
//! Every function here is a line-for-line port of the Solidity library of the
//! same name, using the same rounding direction at every step, so that
//! off-chain quotes match on-chain execution to the wei. Where the Solidity
//! code reverts, the Rust port returns `None`.

pub mod full_math;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_math;

use alloy::primitives::U256;

/// `2^96`, the fixed-point scale of `sqrtPriceX96` (`FixedPoint96.Q96`).
pub const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);

/// Largest value representable as a Solidity `uint160`.
pub const MAX_U160: U256 = U256::from_limbs([u64::MAX, u64::MAX, u32::MAX as u64, 0]);
