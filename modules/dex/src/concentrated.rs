//! Tick-based concentrated-liquidity pools: Uniswap V3 and V3-compatible
//! forks (Pancake V3, Sushi V3, Aerodrome Slipstream, …).

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
    /// ([`GasEstimates::per_initialized_tick`](kairos_core::GasEstimates::per_initialized_tick)).
    pub initialized_ticks_crossed: u32,
}

/// Liquidity at an initialized tick: the `liquidityGross` and `liquidityNet`
/// fields of the pool's `ticks(int24)` (also returned by TickLens).
///
/// `gross` decides whether the tick is initialized, which sets swap step
/// boundaries even when `net` is zero; `net` is the change in active
/// liquidity when the price crosses the tick upward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickLiquidity {
    /// Total liquidity of positions that use this tick as a boundary.
    pub gross: u128,
    /// Liquidity added when crossing left to right (removed right to left).
    pub net: i128,
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
///
/// # Keeping state current
///
/// Apply each `Swap` event with [`Self::set_slot0`] and each `Mint` / `Burn`
/// with [`Self::apply_liquidity_delta`], in log order, so state stays exact
/// without re-reading the pool.
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
    /// Initialized ticks (`gross > 0`).
    ticks: BTreeMap<i32, TickLiquidity>,
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
    /// `ticks` yields every initialized tick in `coverage`. `coverage` is the
    /// range the data is complete for; pass `MIN_TICK..=MAX_TICK` if all
    /// ticks were loaded.
    ///
    /// # Errors
    ///
    /// [`KairosError::ConfigError`] if a tick is not a multiple of the tick
    /// spacing, lies outside `coverage`, is uninitialized (`gross == 0`), or
    /// has `|net| > gross`.
    pub fn set_ticks(
        &mut self,
        ticks: impl IntoIterator<Item = (i32, TickLiquidity)>,
        coverage: RangeInclusive<i32>,
    ) -> Result<()> {
        let mut map = BTreeMap::new();
        for (tick, info) in ticks {
            if tick % self.tick_spacing != 0 || !coverage.contains(&tick) {
                return Err(KairosError::ConfigError(format!(
                    "pool {}: tick {tick} is not a multiple of {} within {coverage:?}",
                    self.address, self.tick_spacing
                )));
            }
            if info.gross == 0 || info.net.unsigned_abs() > info.gross {
                return Err(KairosError::ConfigError(format!(
                    "pool {}: tick {tick} has invalid liquidity {info:?}",
                    self.address
                )));
            }
            map.insert(tick, info);
        }
        self.ticks = map;
        self.tick_coverage = Some(coverage);
        Ok(())
    }

    /// Applies a `Mint` (`delta = amount`) or `Burn` (`delta = -amount`) of a
    /// position over `[tick_lower, tick_upper)`, as
    /// `UniswapV3Pool._modifyPosition` does.
    ///
    /// Updates both boundary ticks and, if the current tick is in range, the
    /// active liquidity. A boundary tick outside the loaded coverage is not
    /// recorded, since swaps never read past the coverage. Apply events in
    /// log order: the in-range check uses the tick from the latest
    /// [`Self::set_slot0`].
    ///
    /// On error the pool is left unchanged.
    ///
    /// # Errors
    ///
    /// - [`KairosError::ConfigError`] if `tick_lower >= tick_upper`, either
    ///   tick is not a multiple of the tick spacing, or either lies outside
    ///   `MIN_TICK..=MAX_TICK`.
    /// - [`KairosError::StateDesync`] if the delta would push a tick's or the
    ///   pool's liquidity below zero or past its maximum. The local state no
    ///   longer matches the chain; reload the pool.
    pub fn apply_liquidity_delta(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        delta: i128,
    ) -> Result<()> {
        let valid_tick = |t: i32| t % self.tick_spacing == 0 && (MIN_TICK..=MAX_TICK).contains(&t);
        if tick_lower >= tick_upper || !valid_tick(tick_lower) || !valid_tick(tick_upper) {
            return Err(KairosError::ConfigError(format!(
                "pool {}: invalid position range [{tick_lower}, {tick_upper}) for spacing {}",
                self.address, self.tick_spacing
            )));
        }
        if delta == 0 {
            // A zero-amount burn only collects fees; liquidity is unchanged.
            return Ok(());
        }

        // Compute everything before writing so a failure changes nothing.
        let lower = self.updated_tick(tick_lower, delta, false)?;
        let upper = self.updated_tick(tick_upper, delta, true)?;
        let liquidity = if (tick_lower..tick_upper).contains(&self.tick) {
            add_liquidity_delta(self.liquidity, delta).ok_or_else(|| {
                self.desync(format!("active liquidity {} + {delta}", self.liquidity))
            })?
        } else {
            self.liquidity
        };

        for (tick, info) in [(tick_lower, lower), (tick_upper, upper)] {
            match info {
                Some(info) if info.gross == 0 => {
                    self.ticks.remove(&tick);
                }
                Some(info) => {
                    self.ticks.insert(tick, info);
                }
                None => {}
            }
        }
        self.liquidity = liquidity;
        Ok(())
    }

    /// Port of `Tick.update` for one position boundary. `None` if the tick is
    /// outside the loaded coverage and so not tracked; `gross == 0` in the
    /// result means the tick becomes uninitialized.
    fn updated_tick(&self, tick: i32, delta: i128, upper: bool) -> Result<Option<TickLiquidity>> {
        if !self
            .tick_coverage
            .as_ref()
            .is_some_and(|c| c.contains(&tick))
        {
            return Ok(None);
        }
        let current = self
            .ticks
            .get(&tick)
            .copied()
            .unwrap_or(TickLiquidity { gross: 0, net: 0 });
        let gross = add_liquidity_delta(current.gross, delta)
            .ok_or_else(|| self.desync(format!("tick {tick} gross {} + {delta}", current.gross)))?;
        let net = if upper {
            current.net.checked_sub(delta)
        } else {
            current.net.checked_add(delta)
        }
        .ok_or_else(|| self.desync(format!("tick {tick} net {} overflows", current.net)))?;
        Ok(Some(TickLiquidity { gross, net }))
    }

    fn desync(&self, reason: String) -> KairosError {
        KairosError::StateDesync {
            pool: self.address,
            reason,
        }
    }

    /// Current `(sqrt_price_x96, tick, liquidity)`.
    pub fn slot0(&self) -> (U256, i32, u128) {
        (self.sqrt_price_x96, self.tick, self.liquidity)
    }

    /// Pool fee in hundredths of a basis point.
    pub fn fee_pips(&self) -> u32 {
        self.fee_pips
    }

    /// Replaces the swap fee, for pools whose fee changes without an event
    /// (Aerodrome Slipstream's dynamic fee module).
    ///
    /// # Errors
    ///
    /// [`KairosError::ConfigError`] if `fee_pips >= FEE_DENOMINATOR`; the fee
    /// is left unchanged.
    pub fn set_fee_pips(&mut self, fee_pips: u32) -> Result<()> {
        if fee_pips >= FEE_DENOMINATOR {
            return Err(KairosError::ConfigError(format!(
                "pool {}: invalid fee {fee_pips}",
                self.address
            )));
        }
        self.fee_pips = fee_pips;
        Ok(())
    }

    /// Distance between usable ticks.
    pub fn tick_spacing(&self) -> i32 {
        self.tick_spacing
    }

    /// Range the loaded tick data is complete for, or `None` if no ticks have
    /// been loaded. Swaps that would reach past it return `None`.
    pub fn tick_coverage(&self) -> Option<RangeInclusive<i32>> {
        self.tick_coverage.clone()
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
                    let mut liquidity_net = self.ticks.get(&tick_next)?.net;
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
    const USDC: Address = Address::repeat_byte(0xa0);
    const WETH: Address = Address::repeat_byte(0xc0);

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
                (-600, tl(L, L as i128)),
                (-60, tl(L, L as i128)),
                (60, tl(L, -(L as i128))),
                (600, tl(L, -(L as i128))),
            ],
            MIN_TICK..=MAX_TICK,
        )
        .unwrap();
        pool
    }

    fn fixture() -> ConcentratedLiquidityPool {
        pool()
    }

    #[test]
    fn fee_update_changes_quotes_and_rejects_invalid() {
        let mut pool = pool();
        let before = pool.swap(true, exact_in(1_000_000)).unwrap().amount_out;
        pool.set_fee_pips(154).unwrap();
        let after = pool.swap(true, exact_in(1_000_000)).unwrap().amount_out;
        assert!(after > before);
        assert!(pool.set_fee_pips(FEE_DENOMINATOR).is_err());
        assert_eq!(pool.fee_pips(), 154);
    }

    fn tl(gross: u128, net: i128) -> TickLiquidity {
        TickLiquidity { gross, net }
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
        pool.set_ticks(
            [(-60, tl(L, L as i128)), (60, tl(L, -(L as i128)))],
            -60..=60,
        )
        .unwrap();
        // Even a tiny swap searches the whole bitmap word, which exceeds -60..=60.
        assert!(pool.swap(true, exact_in(1_000)).is_none());
    }

    #[test]
    fn set_ticks_validates_spacing_and_coverage() {
        let mut pool = pool();
        assert!(
            pool.set_ticks([(61, tl(1, 1))], MIN_TICK..=MAX_TICK)
                .is_err()
        );
        assert!(pool.set_ticks([(120, tl(1, 1))], -60..=60).is_err());
        // Uninitialized, or net larger than gross.
        assert!(
            pool.set_ticks([(60, tl(0, 0))], MIN_TICK..=MAX_TICK)
                .is_err()
        );
        assert!(
            pool.set_ticks([(60, tl(1, -2))], MIN_TICK..=MAX_TICK)
                .is_err()
        );
    }

    #[test]
    fn mint_in_range_matches_pool_loaded_with_the_result() {
        let mut pool = pool();
        pool.apply_liquidity_delta(-120, 120, L as i128).unwrap();

        let mut expected = fixture();
        expected.set_slot0(Q96, 0, 3 * L);
        expected
            .set_ticks(
                [
                    (-600, tl(L, L as i128)),
                    (-120, tl(L, L as i128)),
                    (-60, tl(L, L as i128)),
                    (60, tl(L, -(L as i128))),
                    (120, tl(L, -(L as i128))),
                    (600, tl(L, -(L as i128))),
                ],
                MIN_TICK..=MAX_TICK,
            )
            .unwrap();
        assert_eq!(pool, expected);
    }

    #[test]
    fn mint_out_of_range_leaves_active_liquidity() {
        let mut pool = pool();
        pool.apply_liquidity_delta(600, 1200, L as i128).unwrap();
        assert_eq!(pool.liquidity, 2 * L);
        // Tick 600 already had -L; the new position adds +L.
        assert_eq!(pool.ticks[&600], tl(2 * L, 0));
        assert_eq!(pool.ticks[&1200], tl(L, -(L as i128)));
    }

    #[test]
    fn range_is_lower_inclusive_upper_exclusive() {
        let mut at_lower = pool();
        at_lower.apply_liquidity_delta(0, 60, L as i128).unwrap();
        assert_eq!(at_lower.liquidity, 3 * L);

        let mut at_upper = pool();
        at_upper.apply_liquidity_delta(-60, 0, L as i128).unwrap();
        assert_eq!(at_upper.liquidity, 2 * L);
    }

    #[test]
    fn burn_undoes_mint_and_uninitializes_ticks() {
        let mut pool = pool();
        pool.apply_liquidity_delta(-180, 240, 7).unwrap();
        pool.apply_liquidity_delta(-180, 240, -7).unwrap();
        assert_eq!(pool, fixture());
    }

    #[test]
    fn tick_with_zero_net_stays_initialized() {
        // Position [60, 120) cancels tick 60's net, but gross stays 2L, so
        // swaps still stop there exactly as on chain.
        let mut pool = pool();
        pool.apply_liquidity_delta(60, 120, L as i128).unwrap();
        assert_eq!(pool.ticks[&60], tl(2 * L, 0));
    }

    #[test]
    fn burning_existing_position_changes_quotes() {
        let mut pool = pool();
        // Large enough for price impact to show, small enough to stay above -60.
        let amount = U256::from(1_000_000_000_000_000u64);
        let before = pool.get_amount_out(USDC, WETH, amount);
        pool.apply_liquidity_delta(-60, 60, -(L as i128)).unwrap();
        assert_eq!(pool.liquidity, L);
        assert!(!pool.ticks.contains_key(&-60) && !pool.ticks.contains_key(&60));
        let after = pool.get_amount_out(USDC, WETH, amount);
        assert!(after < before, "thinner liquidity must quote worse");
    }

    #[test]
    fn over_burn_is_desync_and_changes_nothing() {
        let mut pool = pool();
        // Tick -60 holds L; burning 2L from [-60, 60) is impossible.
        let err = pool
            .apply_liquidity_delta(-60, 60, -2 * (L as i128))
            .unwrap_err();
        assert!(matches!(err, KairosError::StateDesync { .. }));
        assert_eq!(pool, fixture());

        // Active liquidity underflow alone is caught too.
        let mut no_ticks = fixture();
        no_ticks.set_ticks([], 1200..=1200).unwrap();
        let err = no_ticks
            .apply_liquidity_delta(-60, 60, -3 * (L as i128))
            .unwrap_err();
        assert!(matches!(err, KairosError::StateDesync { .. }));
    }

    #[test]
    fn ticks_outside_coverage_are_not_recorded() {
        let mut pool = pool();
        pool.set_ticks(
            [(-60, tl(L, L as i128)), (60, tl(L, -(L as i128)))],
            -6000..=6000,
        )
        .unwrap();
        pool.apply_liquidity_delta(-60, 12000, L as i128).unwrap();
        assert_eq!(pool.ticks[&-60], tl(2 * L, 2 * L as i128));
        assert!(!pool.ticks.contains_key(&12000));
        assert_eq!(pool.liquidity, 3 * L);
    }

    #[test]
    fn zero_delta_is_noop() {
        let mut pool = pool();
        pool.apply_liquidity_delta(-60, 60, 0).unwrap();
        assert_eq!(pool, fixture());
    }

    #[test]
    fn invalid_position_range_is_rejected() {
        let mut pool = pool();
        for (lo, hi) in [
            (60, 60),
            (120, 60),
            (-61, 60),
            (-60, 61),
            (MIN_TICK - 60, 0),
        ] {
            assert!(
                matches!(
                    pool.apply_liquidity_delta(lo, hi, 1),
                    Err(KairosError::ConfigError(_))
                ),
                "[{lo}, {hi})"
            );
        }
        assert_eq!(pool, fixture());
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
