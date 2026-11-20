//! Tick-based concentrated-liquidity pools: Uniswap V3 and V3-compatible
//! forks (Pancake V3, Sushi V3, …).

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use alloy::primitives::{Address, Bytes, I256, U256, aliases::U24};
use kairos_core::{AmmKind, FEE_DENOMINATOR, KairosError, PoolId, ProtocolId, Result, SwapStep};

use crate::math::swap_math::compute_swap_step;
use crate::math::tick_math::{
    MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK, get_sqrt_ratio_at_tick,
    get_tick_at_sqrt_ratio,
};
use crate::{Pool, smallest_sufficient_input};

/// Result of simulating a swap with [`ConcentratedLiquidityPool::swap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapOutcome {
    /// Total input paid by the trader, fee included.
    pub amount_in: U256,
    /// Total output received by the trader.
    pub amount_out: U256,
    /// Pool sqrt price after the swap.
    pub sqrt_price_x96: U256,
    /// Pool tick after the swap.
    pub tick: i32,
    /// Active liquidity after the swap.
    pub liquidity: u128,
    /// Number of initialized ticks crossed; drives the per-tick gas estimate
    /// ([`kairos_core::gas::UNIV3_PER_TICK_GAS`]).
    pub initialized_ticks_crossed: u32,
}

/// A Uniswap V3-style pool.
///
/// [`Self::swap`] replays `UniswapV3Pool.swap` step by step with the exact
/// integer math, including word-by-word tick-bitmap traversal (step boundaries
/// affect rounding), so quotes match on-chain execution to the wei.
///
/// # Tick data
///
/// The pool needs every *initialized* tick (`liquidityGross > 0`, including
/// ticks whose `liquidityNet` is zero) over the range a swap may reach. Load
/// them with [`Self::set_ticks`], stating the range the data is complete for.
/// A swap that would need ticks outside that range returns `None` rather than
/// a silently wrong quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcentratedLiquidityPool {
    address: Address,
    protocol_id: ProtocolId,
    tokens: [Address; 2],
    fee_pips: u32,
    tick_spacing: i32,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    /// Initialized ticks -> `liquidityNet`.
    ticks: BTreeMap<i32, i128>,
    /// Range of ticks for which `ticks` is known to be complete.
    tick_coverage: Option<RangeInclusive<i32>>,
}

impl ConcentratedLiquidityPool {
    /// Creates an uninitialized pool; call [`Self::set_slot0`] and
    /// [`Self::set_ticks`] before quoting.
    ///
    /// `fee_pips` is the pool's `fee()` in hundredths of a basis point
    /// (`500`, `3000`, `10000`, …).
    ///
    /// # Errors
    ///
    /// - [`KairosError::InvalidTokenPair`] unless `token0 < token1`.
    /// - [`KairosError::ConfigError`] if `fee_pips >= 1e6` or
    ///   `tick_spacing <= 0`.
    pub fn new(
        address: Address,
        protocol_id: ProtocolId,
        token0: Address,
        token1: Address,
        fee_pips: u32,
        tick_spacing: i32,
    ) -> Result<Self> {
        if token0 >= token1 {
            return Err(KairosError::InvalidTokenPair {
                token_in: token0,
                token_out: token1,
            });
        }
        if fee_pips >= FEE_DENOMINATOR || tick_spacing <= 0 {
            return Err(KairosError::ConfigError(format!(
                "pool {address}: invalid fee {fee_pips} or tick spacing {tick_spacing}"
            )));
        }
        Ok(Self {
            address,
            protocol_id,
            tokens: [token0, token1],
            fee_pips,
            tick_spacing,
            sqrt_price_x96: U256::ZERO,
            tick: 0,
            liquidity: 0,
            ticks: BTreeMap::new(),
            tick_coverage: None,
        })
    }

    /// Sets the active state, from `slot0()` + `liquidity()` or a `Swap` event.
    ///
    /// Use the on-chain `tick` rather than deriving it from the price: at an
    /// exact tick boundary the pool's tick can legitimately be one below.
    pub fn set_slot0(&mut self, sqrt_price_x96: U256, tick: i32, liquidity: u128) {
        self.sqrt_price_x96 = sqrt_price_x96;
        self.tick = tick;
        self.liquidity = liquidity;
    }

    /// Replaces the tick data.
    ///
    /// `ticks` yields `(tick, liquidityNet)` for every initialized tick in
    /// `coverage`. `coverage` is the range the data is complete for; pass
    /// `MIN_TICK..=MAX_TICK` if all ticks were loaded.
    ///
    /// # Errors
    ///
    /// [`KairosError::ConfigError`] if a tick is not a multiple of the tick
    /// spacing or lies outside `coverage`.
    pub fn set_ticks(
        &mut self,
        ticks: impl IntoIterator<Item = (i32, i128)>,
        coverage: RangeInclusive<i32>,
    ) -> Result<()> {
        let mut map = BTreeMap::new();
        for (tick, liquidity_net) in ticks {
            if tick % self.tick_spacing != 0 || !coverage.contains(&tick) {
                return Err(KairosError::ConfigError(format!(
                    "pool {}: tick {tick} is not a multiple of {} within {coverage:?}",
                    self.address, self.tick_spacing
                )));
            }
            map.insert(tick, liquidity_net);
        }
        self.ticks = map;
        self.tick_coverage = Some(coverage);
        Ok(())
    }

    /// Current `(sqrt_price_x96, tick, liquidity)`.
    pub fn slot0(&self) -> (U256, i32, u128) {
        (self.sqrt_price_x96, self.tick, self.liquidity)
    }

    /// Pool fee in hundredths of a basis point.
    pub fn fee_pips(&self) -> u32 {
        self.fee_pips
    }

    /// Simulates `UniswapV3Pool.swap` with the price limit the executor uses
    /// (`MIN_SQRT_RATIO + 1` / `MAX_SQRT_RATIO - 1`).
    ///
    /// `amount_specified` is positive for exact input and negative for exact
    /// output, as on chain.
    ///
    /// Returns `None` if the pool is uninitialized, the amount is zero, the
    /// swap needs tick data outside the loaded coverage, or the pool runs out
    /// of liquidity before filling the full amount (on chain this would be a
    /// partial fill, which Kairos treats as unfillable).
    pub fn swap(&self, zero_for_one: bool, amount_specified: I256) -> Option<SwapOutcome> {
        if amount_specified.is_zero() || self.sqrt_price_x96.is_zero() {
            return None;
        }
        let price_limit = if zero_for_one {
            MIN_SQRT_RATIO + U256::from(1)
        } else {
            MAX_SQRT_RATIO - U256::from(1)
        };
        let limit_valid = if zero_for_one {
            price_limit < self.sqrt_price_x96
        } else {
            price_limit > self.sqrt_price_x96
        };
        if !limit_valid {
            return None;
        }

        let exact_in = amount_specified.is_positive();
        let mut remaining = amount_specified;
        let mut calculated = I256::ZERO;
        let mut sqrt_price = self.sqrt_price_x96;
        let mut tick = self.tick;
        let mut liquidity = self.liquidity;
        let mut crossed = 0u32;

        while !remaining.is_zero() && sqrt_price != price_limit {
            let sqrt_price_start = sqrt_price;
            let (tick_next, initialized) =
                self.next_initialized_tick_within_one_word(tick, zero_for_one)?;
            let tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
            let sqrt_price_next = get_sqrt_ratio_at_tick(tick_next)?;

            let target = if (zero_for_one && sqrt_price_next < price_limit)
                || (!zero_for_one && sqrt_price_next > price_limit)
            {
                price_limit
            } else {
                sqrt_price_next
            };

            let step = compute_swap_step(sqrt_price, target, liquidity, remaining, self.fee_pips)?;
            sqrt_price = step.sqrt_price_next_x96;

            let paid = I256::try_from(step.amount_in.checked_add(step.fee_amount)?).ok()?;
            let received = I256::try_from(step.amount_out).ok()?;
            if exact_in {
                remaining = remaining.checked_sub(paid)?;
                calculated = calculated.checked_sub(received)?;
            } else {
                remaining = remaining.checked_add(received)?;
                calculated = calculated.checked_add(paid)?;
            }

            if sqrt_price == sqrt_price_next {
                if initialized {
                    let mut liquidity_net = *self.ticks.get(&tick_next)?;
                    if zero_for_one {
                        liquidity_net = liquidity_net.checked_neg()?;
                    }
                    liquidity = add_liquidity_delta(liquidity, liquidity_net)?;
                    crossed += 1;
                }
                tick = if zero_for_one {
                    tick_next - 1
                } else {
                    tick_next
                };
            } else if sqrt_price != sqrt_price_start {
                tick = get_tick_at_sqrt_ratio(sqrt_price)?;
            }
        }

        if !remaining.is_zero() {
            return None;
        }

        let specified = amount_specified.unsigned_abs();
        let calculated = calculated.unsigned_abs();
        let (amount_in, amount_out) = if exact_in {
            (specified, calculated)
        } else {
            (calculated, specified)
        };
        Some(SwapOutcome {
            amount_in,
            amount_out,
            sqrt_price_x96: sqrt_price,
            tick,
            liquidity,
            initialized_ticks_crossed: crossed,
        })
    }

    /// Port of `TickBitmap.nextInitializedTickWithinOneWord` over the loaded
    /// tick set. Returns `None` if the searched word is not fully covered by
    /// the loaded tick data.
    fn next_initialized_tick_within_one_word(&self, tick: i32, lte: bool) -> Option<(i32, bool)> {
        let spacing = self.tick_spacing;
        let compressed = tick.div_euclid(spacing);

        let (lo, hi) = if lte {
            let word_start = compressed - compressed.rem_euclid(256);
            (word_start * spacing, compressed * spacing)
        } else {
            let next = compressed + 1;
            let word_end = next - next.rem_euclid(256) + 255;
            (next * spacing, word_end * spacing)
        };

        let coverage = self.tick_coverage.as_ref()?;
        if !coverage.contains(&lo.max(MIN_TICK)) || !coverage.contains(&hi.min(MAX_TICK)) {
            return None;
        }

        let mut in_word = self.ticks.range(lo..=hi);
        let found = if lte {
            in_word.next_back()
        } else {
            in_word.next()
        };
        Some(match found {
            Some((&t, _)) => (t, true),
            None => (if lte { lo } else { hi }, false),
        })
    }

    fn direction(&self, token_in: Address, token_out: Address) -> Option<bool> {
        let [token0, token1] = self.tokens;
        if token_in == token0 && token_out == token1 {
            Some(true)
        } else if token_in == token1 && token_out == token0 {
            Some(false)
        } else {
            None
        }
    }
}

/// `LiquidityMath.addDelta`: returns `None` on under/overflow.
fn add_liquidity_delta(liquidity: u128, delta: i128) -> Option<u128> {
    if delta < 0 {
        liquidity.checked_sub(delta.unsigned_abs())
    } else {
        liquidity.checked_add(delta as u128)
    }
}

impl Pool for ConcentratedLiquidityPool {
    fn id(&self) -> PoolId {
        PoolId {
            address: self.address,
            kind: AmmKind::ConcentratedLiquidity,
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
        let zero_for_one = self.direction(token_in, token_out)?;
        let outcome = self.swap(zero_for_one, I256::try_from(amount_in).ok()?)?;
        (!outcome.amount_out.is_zero()).then_some(outcome.amount_out)
    }

    /// Uses an exact-output simulation as a starting point, then adjusts it so
    /// the result is the smallest *exact-input* amount that yields
    /// `amount_out`. The executor always swaps exact-input, and the two modes
    /// can differ by a few wei of rounding.
    fn get_amount_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_out: U256,
    ) -> Option<U256> {
        let zero_for_one = self.direction(token_in, token_out)?;
        if amount_out.is_zero() {
            return None;
        }
        let hint = self
            .swap(zero_for_one, -I256::try_from(amount_out).ok()?)?
            .amount_in;
        smallest_sufficient_input(hint, amount_out, |x| {
            self.get_amount_out(token_in, token_out, x)
        })
    }

    fn swap_step(
        &self,
        token_in: Address,
        token_out: Address,
        min_amount_out: U256,
    ) -> Result<SwapStep> {
        if self.direction(token_in, token_out).is_none() {
            return Err(KairosError::InvalidTokenPair {
                token_in,
                token_out,
            });
        }
        let step = SwapStep {
            protocol_id: self.protocol_id,
            // The executor ignores feeP for this family and requires it to be 0;
            // the pool charges its own fee.
            fee_ppm: U24::ZERO,
            pool: self.address,
            token_out,
            min_amount_out,
            data: Bytes::new(),
        };
        step.validate(AmmKind::ConcentratedLiquidity)?;
        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Q96;
    use crate::math::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
    use kairos_core::addresses::{USDC, WETH};

    const UNI_V3: ProtocolId = ProtocolId(2);
    const L: u128 = 1_000_000_000_000_000_000; // 1e18

    /// Price 1.0 (tick 0), 0.30% fee, spacing 60, liquidity L in [-600, 600),
    /// and an extra L in [-60, 60) so small swaps cross ticks.
    fn pool() -> ConcentratedLiquidityPool {
        let mut pool = ConcentratedLiquidityPool::new(
            Address::repeat_byte(0xbb),
            UNI_V3,
            USDC,
            WETH,
            3000,
            60,
        )
        .unwrap();
        pool.set_slot0(Q96, 0, 2 * L);
        pool.set_ticks(
            [
                (-600, L as i128),
                (-60, L as i128),
                (60, -(L as i128)),
                (600, -(L as i128)),
            ],
            MIN_TICK..=MAX_TICK,
        )
        .unwrap();
        pool
    }

    fn exact_in(amount: u128) -> I256 {
        I256::try_from(amount).unwrap()
    }

    #[test]
    fn small_swap_stays_in_range_and_matches_closed_form() {
        let pool = pool();
        let amount = 1_000_000u128;
        let out = pool.swap(true, exact_in(amount)).unwrap();
        assert_eq!(out.initialized_ticks_crossed, 0);
        assert_eq!(out.amount_in, U256::from(amount));

        // Single-range closed form, same rounding as SwapMath.
        let after_fee = U256::from(amount) * U256::from(997_000) / U256::from(1_000_000);
        let next = crate::math::sqrt_price_math::get_next_sqrt_price_from_input(
            Q96,
            2 * L,
            after_fee,
            true,
        )
        .unwrap();
        assert_eq!(out.sqrt_price_x96, next);
        assert_eq!(
            out.amount_out,
            get_amount1_delta(next, Q96, 2 * L, false).unwrap()
        );
    }

    #[test]
    fn swap_crosses_ticks_and_updates_liquidity() {
        let pool = pool();
        // Enough token1 to push price past tick 60 (but not 600).
        let to_60 =
            get_amount1_delta(Q96, get_sqrt_ratio_at_tick(60).unwrap(), 2 * L, true).unwrap();
        let amount = to_60 * U256::from(2);
        let out = pool.swap(false, I256::try_from(amount).unwrap()).unwrap();
        assert_eq!(out.initialized_ticks_crossed, 1);
        assert_eq!(out.liquidity, L);
        assert!(out.tick >= 60 && out.tick < 600);
    }

    #[test]
    fn crossing_downward_negates_liquidity_net() {
        let pool = pool();
        let to_minus_60 =
            get_amount0_delta(get_sqrt_ratio_at_tick(-60).unwrap(), Q96, 2 * L, true).unwrap();
        let out = pool
            .swap(true, I256::try_from(to_minus_60 * U256::from(2)).unwrap())
            .unwrap();
        assert_eq!(out.initialized_ticks_crossed, 1);
        assert_eq!(out.liquidity, L);
        assert!(out.tick < -60);
    }

    #[test]
    fn running_out_of_liquidity_is_unfillable() {
        // All liquidity ends at tick 600; a huge swap cannot be filled.
        let pool = pool();
        assert!(pool.swap(false, exact_in(L * 1_000)).is_none());
    }

    #[test]
    fn missing_tick_coverage_is_refused() {
        let mut pool = pool();
        pool.set_ticks([(-60, L as i128), (60, -(L as i128))], -60..=60)
            .unwrap();
        // Even a tiny swap searches the whole bitmap word, which exceeds -60..=60.
        assert!(pool.swap(true, exact_in(1_000)).is_none());
    }

    #[test]
    fn set_ticks_validates_spacing_and_coverage() {
        let mut pool = pool();
        assert!(pool.set_ticks([(61, 1)], MIN_TICK..=MAX_TICK).is_err());
        assert!(pool.set_ticks([(120, 1)], -60..=60).is_err());
    }

    #[test]
    fn exact_out_and_exact_in_agree() {
        let pool = pool();
        let want = U256::from(5_000_000_000_000_000u64);
        let out = pool.swap(true, -I256::try_from(want).unwrap()).unwrap();
        assert_eq!(out.amount_out, want);
        let back = pool
            .swap(true, I256::try_from(out.amount_in).unwrap())
            .unwrap();
        assert!(back.amount_out >= want);
    }

    #[test]
    fn amount_in_is_minimal_and_sufficient() {
        let pool = pool();
        // 2e16 crosses tick -60; the pool holds only ~3.3e16 token1 below tick 0.
        assert!(
            pool.get_amount_in(USDC, WETH, U256::from(50_000_000_000_000_000u64))
                .is_none()
        );
        for want in [
            1u64,
            1_000_000,
            5_000_000_000_000_000,
            20_000_000_000_000_000,
        ] {
            let want = U256::from(want);
            let needed = pool.get_amount_in(USDC, WETH, want).unwrap();
            assert!(pool.get_amount_out(USDC, WETH, needed).unwrap() >= want);
            let short = pool.get_amount_out(USDC, WETH, needed - U256::from(1));
            assert!(
                short.is_none_or(|o| o < want),
                "input {needed} is not minimal for {want}"
            );
        }
    }

    #[test]
    fn direction_follows_token_order() {
        let pool = pool();
        let amount = U256::from(1_000_000u64);
        assert_eq!(
            pool.get_amount_out(USDC, WETH, amount),
            Some(
                pool.swap(true, I256::try_from(amount).unwrap())
                    .unwrap()
                    .amount_out
            )
        );
        assert_eq!(
            pool.get_amount_out(WETH, USDC, amount),
            Some(
                pool.swap(false, I256::try_from(amount).unwrap())
                    .unwrap()
                    .amount_out
            )
        );
        assert!(pool.get_amount_out(USDC, USDC, amount).is_none());
    }

    #[test]
    fn uninitialized_pool_cannot_quote() {
        let pool =
            ConcentratedLiquidityPool::new(Address::ZERO, UNI_V3, USDC, WETH, 3000, 60).unwrap();
        assert!(pool.get_amount_out(USDC, WETH, U256::from(1)).is_none());
    }

    #[test]
    fn swap_step_has_zero_fee_and_no_calldata() {
        let step = pool().swap_step(USDC, WETH, U256::from(9)).unwrap();
        assert_eq!(step.fee_ppm, U24::ZERO);
        assert!(step.data.is_empty());
        assert_eq!(step.token_out, WETH);
        assert_eq!(step.protocol_id, UNI_V3);
    }
}
