//! `x * y = k` pools: Uniswap V2 and its forks (SushiSwap, Camelot V2, …).

use alloy::primitives::{Address, Bytes, U256, aliases::U24};
use kairos_core::{AmmKind, FEE_DENOMINATOR, KairosError, PoolId, ProtocolId, Result, SwapStep};

use crate::Pool;
use crate::math::full_math::{mul_div, mul_div_rounding_up};

/// A constant-product pair.
///
/// Quotes replicate `KairosExecutorBase._swapConstantProduct` exactly, not
/// the router's `getAmountOut`. The executor prices each hop itself against
/// live reserves using [`Self::fee_ppm`], so this is the formula that decides
/// what the route actually receives:
///
/// ```text
/// in_after_fee = amount_in * (1e6 - fee_ppm)
/// amount_out   = in_after_fee * reserve_out / (reserve_in * 1e6 + in_after_fee)
/// ```
///
/// `fee_ppm` must be at least the pair's real fee (3000 for Uniswap V2), or
/// the pair's own `K` check reverts the swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstantProductPool {
    address: Address,
    protocol_id: ProtocolId,
    tokens: [Address; 2],
    fee_ppm: u32,
    reserve0: U256,
    reserve1: U256,
}

impl ConstantProductPool {
    /// Creates a pool with zero reserves; call [`Self::set_reserves`] before
    /// quoting.
    ///
    /// # Errors
    ///
    /// - [`KairosError::InvalidTokenPair`] unless `token0 < token1`. The
    ///   executor derives swap direction from address order, as V2 factories
    ///   do, so unsorted pairs would be priced backwards.
    /// - [`KairosError::ConfigError`] if `fee_ppm >= FEE_DENOMINATOR`.
    pub fn new(
        address: Address,
        protocol_id: ProtocolId,
        token0: Address,
        token1: Address,
        fee_ppm: u32,
    ) -> Result<Self> {
        if token0 >= token1 {
            return Err(KairosError::InvalidTokenPair {
                token_in: token0,
                token_out: token1,
            });
        }
        if fee_ppm >= FEE_DENOMINATOR {
            return Err(KairosError::ConfigError(format!(
                "pool {address}: fee {fee_ppm} ppm must be below {FEE_DENOMINATOR}"
            )));
        }
        Ok(Self {
            address,
            protocol_id,
            tokens: [token0, token1],
            fee_ppm,
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
        })
    }

    /// Replaces the reserves, e.g. from a `Sync` event or `getReserves()`.
    pub fn set_reserves(&mut self, reserve0: U256, reserve1: U256) {
        self.reserve0 = reserve0;
        self.reserve1 = reserve1;
    }

    /// Current `(reserve0, reserve1)`.
    pub fn reserves(&self) -> (U256, U256) {
        (self.reserve0, self.reserve1)
    }

    /// Swap fee in parts-per-million, as sent in [`SwapStep::fee_ppm`].
    pub fn fee_ppm(&self) -> u32 {
        self.fee_ppm
    }

    /// `(reserve_in, reserve_out)` for a swap from `token_in` to `token_out`,
    /// or `None` if the pair does not match this pool.
    fn oriented_reserves(&self, token_in: Address, token_out: Address) -> Option<(U256, U256)> {
        let [token0, token1] = self.tokens;
        if token_in == token0 && token_out == token1 {
            Some((self.reserve0, self.reserve1))
        } else if token_in == token1 && token_out == token0 {
            Some((self.reserve1, self.reserve0))
        } else {
            None
        }
    }
}

impl Pool for ConstantProductPool {
    fn id(&self) -> PoolId {
        PoolId {
            address: self.address,
            kind: AmmKind::ConstantProduct,
            protocol_id: self.protocol_id,
        }
    }

    fn tokens(&self) -> &[Address] {
        &self.tokens
    }

    fn get_amount_out(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256> {
        let (reserve_in, reserve_out) = self.oriented_reserves(token_in, token_out)?;
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return None;
        }
        let in_after_fee = amount_in.checked_mul(U256::from(FEE_DENOMINATOR - self.fee_ppm))?;
        let denominator = reserve_in
            .checked_mul(U256::from(FEE_DENOMINATOR))?
            .checked_add(in_after_fee)?;
        let amount_out = mul_div(in_after_fee, reserve_out, denominator)?;
        // The pair rejects zero-output swaps.
        (!amount_out.is_zero()).then_some(amount_out)
    }

    /// Smallest input whose [`get_amount_out`](Pool::get_amount_out) is at
    /// least `amount_out`:
    /// `ceil(amount_out * reserve_in * 1e6 / ((reserve_out - amount_out) * (1e6 - fee)))`.
    fn get_amount_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_out: U256,
    ) -> Option<U256> {
        let (reserve_in, reserve_out) = self.oriented_reserves(token_in, token_out)?;
        if amount_out.is_zero() || reserve_in.is_zero() || amount_out >= reserve_out {
            return None;
        }
        let numerator = reserve_in.checked_mul(U256::from(FEE_DENOMINATOR))?;
        let denominator =
            (reserve_out - amount_out).checked_mul(U256::from(FEE_DENOMINATOR - self.fee_ppm))?;
        mul_div_rounding_up(amount_out, numerator, denominator)
    }

    fn swap_step(
        &self,
        token_in: Address,
        token_out: Address,
        min_amount_out: U256,
    ) -> Result<SwapStep> {
        if self.oriented_reserves(token_in, token_out).is_none() {
            return Err(KairosError::InvalidTokenPair {
                token_in,
                token_out,
            });
        }
        let step = SwapStep {
            protocol_id: self.protocol_id,
            fee_ppm: U24::from(self.fee_ppm),
            pool: self.address,
            token_out,
            min_amount_out,
            data: Bytes::new(),
        };
        step.validate(AmmKind::ConstantProduct)?;
        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const USDC: Address = Address::repeat_byte(0xa0);
    const WETH: Address = Address::repeat_byte(0xc0);

    const UNI_V2: ProtocolId = ProtocolId(1);
    const ONE_ETH: u128 = 1_000_000_000_000_000_000;

    /// USDC/WETH at 2000 USDC per ETH: 10M USDC, 5000 ETH, 0.30% fee.
    fn pool() -> ConstantProductPool {
        let mut pool =
            ConstantProductPool::new(Address::repeat_byte(0xaa), UNI_V2, USDC, WETH, 3000).unwrap();
        pool.set_reserves(
            U256::from(10_000_000_000_000u64),
            U256::from(5_000u128 * ONE_ETH),
        );
        pool
    }

    #[test]
    fn rejects_unsorted_tokens_and_bad_fee() {
        assert!(ConstantProductPool::new(Address::ZERO, UNI_V2, WETH, USDC, 3000).is_err());
        assert!(
            ConstantProductPool::new(Address::ZERO, UNI_V2, USDC, WETH, FEE_DENOMINATOR).is_err()
        );
    }

    /// At 3000 ppm the executor formula is algebraically identical to
    /// UniswapV2Library's 997/1000 `getAmountOut`.
    #[test]
    fn matches_uniswap_v2_formula_at_3000_ppm() {
        let pool = pool();
        let (r_usdc, r_weth) = pool.reserves();
        for amount_in in [1u128, 12_345, ONE_ETH, 250 * ONE_ETH] {
            let amount_in = U256::from(amount_in);
            let with_fee = amount_in * U256::from(997);
            let expected = with_fee * r_usdc / (r_weth * U256::from(1000) + with_fee);
            let got = pool.get_amount_out(WETH, USDC, amount_in);
            assert_eq!(got, (!expected.is_zero()).then_some(expected));
        }
    }

    #[test]
    fn one_eth_buys_just_under_2000_usdc() {
        let out = pool()
            .get_amount_out(WETH, USDC, U256::from(ONE_ETH))
            .unwrap();
        assert!(out > U256::from(1_990_000_000u64) && out < U256::from(2_000_000_000u64));
    }

    #[test]
    fn lower_fee_gives_more_output() {
        let mut cheap = ConstantProductPool::new(Address::ZERO, UNI_V2, USDC, WETH, 1000).unwrap();
        let base = pool();
        let (r0, r1) = base.reserves();
        cheap.set_reserves(r0, r1);
        let amount = U256::from(ONE_ETH);
        assert!(
            cheap.get_amount_out(WETH, USDC, amount).unwrap()
                > base.get_amount_out(WETH, USDC, amount).unwrap()
        );
    }

    #[test]
    fn amount_in_is_minimal_and_sufficient() {
        let pool = pool();
        for target in [1u64, 1_000_000, 1_000_000_000, 5_000_000_000_000] {
            let target = U256::from(target);
            let needed = pool.get_amount_in(WETH, USDC, target).unwrap();
            assert!(pool.get_amount_out(WETH, USDC, needed).unwrap() >= target);
            let short = pool.get_amount_out(WETH, USDC, needed - U256::from(1));
            assert!(
                short.is_none_or(|out| out < target),
                "input {needed} is not minimal"
            );
        }
    }

    #[test]
    fn rejects_wrong_pair_zero_amount_and_empty_reserves() {
        let pool = pool();
        let other = Address::repeat_byte(0x11);
        assert!(pool.get_amount_out(other, USDC, U256::from(1)).is_none());
        assert!(pool.get_amount_out(WETH, WETH, U256::from(1)).is_none());
        assert!(pool.get_amount_out(WETH, USDC, U256::ZERO).is_none());
        assert!(pool.get_amount_in(WETH, USDC, pool.reserves().0).is_none());

        let empty = ConstantProductPool::new(Address::ZERO, UNI_V2, USDC, WETH, 3000).unwrap();
        assert!(empty.get_amount_out(WETH, USDC, U256::from(1)).is_none());
    }

    #[test]
    fn swap_step_carries_fee_and_no_calldata() {
        let pool = pool();
        let step = pool.swap_step(WETH, USDC, U256::from(7)).unwrap();
        assert_eq!(step.protocol_id, UNI_V2);
        assert_eq!(step.fee_ppm, U24::from(3000));
        assert_eq!(step.pool, pool.id().address);
        assert_eq!(step.token_out, USDC);
        assert_eq!(step.min_amount_out, U256::from(7));
        assert!(step.data.is_empty());
        assert!(pool.swap_step(WETH, WETH, U256::ZERO).is_err());
    }
}
