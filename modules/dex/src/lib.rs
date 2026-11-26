//! DEX pool models: off-chain pricing that matches on-chain execution.
//!
//! Each pool type implements [`Pool`] for one [`AmmKind`]. Venues within a
//! family (Uniswap V2, SushiSwap, Camelot V2, …) share one implementation and
//! differ only by their [`ProtocolId`](kairos_core::ProtocolId), so adding a
//! fork is configuration, not code.
//!
//! | `AmmKind`               | Type                       | Status       |
//! |-------------------------|----------------------------|--------------|
//! | `ConstantProduct`       | [`ConstantProductPool`]    | implemented  |
//! | `ConcentratedLiquidity` | [`ConcentratedLiquidityPool`] | implemented |
//! | `StableSwap`            | —                          | not yet      |
//! | `WeightedPool`          | —                          | not yet      |
//! | `LiquidityBook`         | —                          | not yet      |
//!
//! Quotes are exact integer math. A pool that cannot price a swap (wrong
//! tokens, empty reserves, missing tick data, insufficient liquidity) returns
//! `None` rather than an approximation.

pub mod concentrated;
pub mod constant_product;
pub mod math;
pub mod registry;

use alloy::primitives::{Address, U256};
use kairos_core::{AmmKind, PoolId, Result, SwapStep};

pub use concentrated::{ConcentratedLiquidityPool, SwapOutcome, TickLiquidity};
pub use constant_product::ConstantProductPool;
pub use registry::PoolRegistry;

/// Common interface of every pool model.
pub trait Pool: Send + Sync {
    /// Address, math family and registry id of the pool.
    fn id(&self) -> PoolId;

    /// Tokens the pool holds. Two-token pools return them sorted.
    fn tokens(&self) -> &[Address];

    /// Output for swapping exactly `amount_in` of `token_in` into `token_out`,
    /// as the executor would receive it.
    ///
    /// `None` if the pool cannot fill the swap: unknown tokens, zero amount,
    /// zero output, missing state, or insufficient liquidity.
    fn get_amount_out(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256>;

    /// Smallest input of `token_in` for which
    /// [`get_amount_out`](Self::get_amount_out) returns at least
    /// `amount_out`. `None` under the same conditions.
    fn get_amount_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_out: U256,
    ) -> Option<U256>;

    /// Builds the executor [`SwapStep`] for this hop, already checked with
    /// [`SwapStep::validate`].
    ///
    /// # Errors
    ///
    /// [`KairosError::InvalidTokenPair`](kairos_core::KairosError::InvalidTokenPair)
    /// if the tokens do not belong to this pool, or
    /// [`KairosError::InvalidSwapStep`](kairos_core::KairosError::InvalidSwapStep)
    /// if the step would revert on chain.
    fn swap_step(
        &self,
        token_in: Address,
        token_out: Address,
        min_amount_out: U256,
    ) -> Result<SwapStep>;

    /// Math family of the pool. Shorthand for `self.id().kind`.
    fn kind(&self) -> AmmKind {
        self.id().kind
    }

    /// Pool address. Shorthand for `self.id().address`.
    fn address(&self) -> Address {
        self.id().address
    }
}

/// Any supported pool, for storing mixed pools without trait objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnyPool {
    /// See [`ConstantProductPool`].
    ConstantProduct(ConstantProductPool),
    /// See [`ConcentratedLiquidityPool`].
    ConcentratedLiquidity(ConcentratedLiquidityPool),
}

impl From<ConstantProductPool> for AnyPool {
    fn from(pool: ConstantProductPool) -> Self {
        AnyPool::ConstantProduct(pool)
    }
}

impl From<ConcentratedLiquidityPool> for AnyPool {
    fn from(pool: ConcentratedLiquidityPool) -> Self {
        AnyPool::ConcentratedLiquidity(pool)
    }
}

impl AnyPool {
    fn inner(&self) -> &dyn Pool {
        match self {
            AnyPool::ConstantProduct(p) => p,
            AnyPool::ConcentratedLiquidity(p) => p,
        }
    }
}

impl Pool for AnyPool {
    fn id(&self) -> PoolId {
        self.inner().id()
    }

    fn tokens(&self) -> &[Address] {
        self.inner().tokens()
    }

    fn get_amount_out(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256> {
        self.inner().get_amount_out(token_in, token_out, amount_in)
    }

    fn get_amount_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_out: U256,
    ) -> Option<U256> {
        self.inner().get_amount_in(token_in, token_out, amount_out)
    }

    fn swap_step(
        &self,
        token_in: Address,
        token_out: Address,
        min_amount_out: U256,
    ) -> Result<SwapStep> {
        self.inner().swap_step(token_in, token_out, min_amount_out)
    }
}

/// Smallest `x` with `quote(x) >= target`, searching outward from `hint`.
///
/// `quote` must be monotonically non-decreasing in its input (true of every
/// AMM). `hint` should be close to the answer; the search gallops away from it
/// and then bisects, so a hint that is off by a few wei costs a few quotes.
pub(crate) fn smallest_sufficient_input(
    hint: U256,
    target: U256,
    quote: impl Fn(U256) -> Option<U256>,
) -> Option<U256> {
    let sufficient = |x: U256| quote(x).is_some_and(|out| out >= target);
    let one = U256::from(1);

    // Bracket the answer: `lo` insufficient (or zero), `hi` sufficient.
    let (mut lo, mut hi);
    if sufficient(hint) {
        hi = hint;
        let mut step = one;
        loop {
            if step >= hi {
                lo = U256::ZERO;
                break;
            }
            let candidate = hi - step;
            if sufficient(candidate) {
                hi = candidate;
                step = step.checked_shl(1)?;
            } else {
                lo = candidate;
                break;
            }
        }
    } else {
        lo = hint;
        let mut step = one;
        loop {
            let candidate = lo.checked_add(step)?;
            if sufficient(candidate) {
                hi = candidate;
                break;
            }
            lo = candidate;
            step = step.checked_shl(1)?;
        }
    }

    while hi - lo > one {
        let mid = lo + (hi - lo) / U256::from(2);
        if sufficient(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn halve(x: U256) -> Option<U256> {
        Some(x / U256::from(2))
    }

    #[test]
    fn smallest_input_from_any_hint() {
        // halve(x) >= 10 first holds at x = 20.
        for hint in [0u64, 1, 19, 20, 21, 1_000_000] {
            assert_eq!(
                smallest_sufficient_input(U256::from(hint), U256::from(10), halve),
                Some(U256::from(20)),
                "hint {hint}"
            );
        }
    }

    #[test]
    fn unreachable_target_is_none() {
        let capped = |x: U256| Some(x.min(U256::from(5)));
        assert_eq!(
            smallest_sufficient_input(U256::from(3), U256::from(10), capped),
            None
        );
    }
}
