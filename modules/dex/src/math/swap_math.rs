//! A single swap step within one liquidity range (`SwapMath.sol`).

use alloy::primitives::{I256, U256};

use super::full_math::{mul_div, mul_div_rounding_up};
use super::sqrt_price_math::{
    get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output,
};

/// Fee denominator for `fee_pips` (hundredths of a basis point).
const ONE_MILLION: u32 = 1_000_000;

/// Outcome of one [`compute_swap_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStepResult {
    /// Sqrt price after the step; equals the target if the range was exhausted.
    pub sqrt_price_next_x96: U256,
    /// Input consumed, excluding the fee.
    pub amount_in: U256,
    /// Output produced.
    pub amount_out: U256,
    /// Fee taken on top of `amount_in`.
    pub fee_amount: U256,
}

/// Swaps as far as possible from `sqrt_price_current_x96` towards
/// `sqrt_price_target_x96` at constant `liquidity` (`SwapMath.computeSwapStep`).
///
/// `amount_remaining` is positive for exact input and negative for exact
/// output, as on chain. `fee_pips` is the pool fee in hundredths of a basis
/// point (`3000` = 0.30%). The swap direction is implied by the two prices.
pub fn compute_swap_step(
    sqrt_price_current_x96: U256,
    sqrt_price_target_x96: U256,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: u32,
) -> Option<SwapStepResult> {
    if fee_pips >= ONE_MILLION {
        return None;
    }
    let zero_for_one = sqrt_price_current_x96 >= sqrt_price_target_x96;
    let exact_in = !amount_remaining.is_negative();
    let remaining_abs = amount_remaining.unsigned_abs();
    let fee_complement = U256::from(ONE_MILLION - fee_pips);
    let million = U256::from(ONE_MILLION);

    let mut amount_in = U256::ZERO;
    let mut amount_out = U256::ZERO;

    let sqrt_price_next_x96 = if exact_in {
        let remaining_less_fee = mul_div(remaining_abs, fee_complement, million)?;
        amount_in = if zero_for_one {
            get_amount0_delta(
                sqrt_price_target_x96,
                sqrt_price_current_x96,
                liquidity,
                true,
            )?
        } else {
            get_amount1_delta(
                sqrt_price_current_x96,
                sqrt_price_target_x96,
                liquidity,
                true,
            )?
        };
        if remaining_less_fee >= amount_in {
            sqrt_price_target_x96
        } else {
            get_next_sqrt_price_from_input(
                sqrt_price_current_x96,
                liquidity,
                remaining_less_fee,
                zero_for_one,
            )?
        }
    } else {
        amount_out = if zero_for_one {
            get_amount1_delta(
                sqrt_price_target_x96,
                sqrt_price_current_x96,
                liquidity,
                false,
            )?
        } else {
            get_amount0_delta(
                sqrt_price_current_x96,
                sqrt_price_target_x96,
                liquidity,
                false,
            )?
        };
        if remaining_abs >= amount_out {
            sqrt_price_target_x96
        } else {
            get_next_sqrt_price_from_output(
                sqrt_price_current_x96,
                liquidity,
                remaining_abs,
                zero_for_one,
            )?
        }
    };

    let reached_target = sqrt_price_target_x96 == sqrt_price_next_x96;

    // If the target was reached, the side computed above is already exact;
    // otherwise (or for the other side) recompute from the final price.
    let input_known = reached_target && exact_in;
    let output_known = reached_target && !exact_in;
    if zero_for_one {
        if !input_known {
            amount_in =
                get_amount0_delta(sqrt_price_next_x96, sqrt_price_current_x96, liquidity, true)?;
        }
        if !output_known {
            amount_out = get_amount1_delta(
                sqrt_price_next_x96,
                sqrt_price_current_x96,
                liquidity,
                false,
            )?;
        }
    } else {
        if !input_known {
            amount_in =
                get_amount1_delta(sqrt_price_current_x96, sqrt_price_next_x96, liquidity, true)?;
        }
        if !output_known {
            amount_out = get_amount0_delta(
                sqrt_price_current_x96,
                sqrt_price_next_x96,
                liquidity,
                false,
            )?;
        }
    }

    // Exact output must never deliver more than requested.
    if !exact_in && amount_out > remaining_abs {
        amount_out = remaining_abs;
    }

    let fee_amount = if exact_in && !reached_target {
        // Range not exhausted: whatever is left of the input is the fee.
        remaining_abs - amount_in
    } else {
        mul_div_rounding_up(amount_in, U256::from(fee_pips), fee_complement)?
    };

    Some(SwapStepResult {
        sqrt_price_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Q96;
    use alloy::primitives::uint;

    const ONE_E18: u128 = 1_000_000_000_000_000_000;

    fn exact_in(amount: u128) -> I256 {
        I256::try_from(amount).unwrap()
    }

    fn exact_out(amount: u128) -> I256 {
        -I256::try_from(amount).unwrap()
    }

    #[test]
    fn exact_in_that_stops_short_spends_everything() {
        // Target far away, small input: all of it is consumed (amount + fee).
        let target = uint!(87150978765690771352898345369_U256); // 1.1 * 2^96
        let r = compute_swap_step(Q96, target, 2 * ONE_E18, exact_in(1_000_000), 600).unwrap();
        assert!(r.sqrt_price_next_x96 > Q96 && r.sqrt_price_next_x96 < target);
        assert_eq!(r.amount_in + r.fee_amount, U256::from(1_000_000));
        assert!(r.amount_out > U256::ZERO);
    }

    #[test]
    fn exact_in_capped_at_target() {
        // Target close by, large input: stops at target, leaves input unspent.
        let target = Q96 + U256::from(1_000_000_000_000_000_000_000u128);
        let r = compute_swap_step(Q96, target, 2 * ONE_E18, exact_in(ONE_E18), 600).unwrap();
        assert_eq!(r.sqrt_price_next_x96, target);
        assert!(r.amount_in + r.fee_amount < U256::from(ONE_E18));
    }

    #[test]
    fn exact_out_never_overdelivers() {
        let target = uint!(72025602285694852357767227579_U256); // 2^96 / 1.1
        let r = compute_swap_step(Q96, target, 2 * ONE_E18, exact_out(1_000_000), 3000).unwrap();
        assert!(r.amount_out <= U256::from(1_000_000));
        assert!(r.sqrt_price_next_x96 < Q96 && r.sqrt_price_next_x96 > target);
    }

    #[test]
    fn invalid_fee_is_rejected() {
        assert!(compute_swap_step(Q96, Q96 + U256::from(1), 1, exact_in(1), ONE_MILLION).is_none());
    }

    #[test]
    fn zero_liquidity_jumps_to_target_for_free() {
        let target = Q96 + U256::from(1_000u64);
        let r = compute_swap_step(Q96, target, 0, exact_in(1_000), 3000).unwrap();
        assert_eq!(r.sqrt_price_next_x96, target);
        assert_eq!(r.amount_in, U256::ZERO);
        assert_eq!(r.amount_out, U256::ZERO);
    }
}
