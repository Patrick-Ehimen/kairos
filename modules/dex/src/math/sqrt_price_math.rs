//! Price movement and token deltas for a fixed liquidity
//! (`SqrtPriceMath.sol`).
//!
//! Rounding always favours the pool: amounts the trader pays round up,
//! amounts the trader receives round down.

use alloy::primitives::U256;

use super::full_math::{div_rounding_up, mul_div, mul_div_rounding_up};
use super::{MAX_U160, Q96};

fn to_u160(value: U256) -> Option<U256> {
    (value <= MAX_U160).then_some(value)
}

/// New sqrt price after adding (`add`) or removing token0 at constant
/// liquidity, rounded up (`getNextSqrtPriceFromAmount0RoundingUp`).
pub fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_price_x96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Option<U256> {
    if amount.is_zero() {
        return Some(sqrt_price_x96);
    }
    let numerator1 = U256::from(liquidity) << 96usize;

    if add {
        // Precise path when `amount * sqrtP` does not overflow.
        if let Some(product) = amount.checked_mul(sqrt_price_x96)
            && let Some(denominator) = numerator1.checked_add(product)
        {
            return to_u160(mul_div_rounding_up(
                numerator1,
                sqrt_price_x96,
                denominator,
            )?);
        }
        // Fallback: L / (L / sqrtP + amount), less precise but cannot overflow.
        let denominator = (numerator1 / sqrt_price_x96).checked_add(amount)?;
        to_u160(div_rounding_up(numerator1, denominator)?)
    } else {
        let product = amount.checked_mul(sqrt_price_x96)?;
        if numerator1 <= product {
            return None;
        }
        to_u160(mul_div_rounding_up(
            numerator1,
            sqrt_price_x96,
            numerator1 - product,
        )?)
    }
}

/// New sqrt price after adding (`add`) or removing token1 at constant
/// liquidity, rounded down (`getNextSqrtPriceFromAmount1RoundingDown`).
pub fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_price_x96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Option<U256> {
    let liquidity = U256::from(liquidity);
    if liquidity.is_zero() {
        return None;
    }

    if add {
        let quotient = if amount <= MAX_U160 {
            (amount << 96usize) / liquidity
        } else {
            mul_div(amount, Q96, liquidity)?
        };
        to_u160(sqrt_price_x96.checked_add(quotient)?)
    } else {
        let quotient = if amount <= MAX_U160 {
            div_rounding_up(amount << 96, liquidity)?
        } else {
            mul_div_rounding_up(amount, Q96, liquidity)?
        };
        if sqrt_price_x96 <= quotient {
            return None;
        }
        Some(sqrt_price_x96 - quotient)
    }
}

/// Sqrt price after swapping in `amount_in` (`getNextSqrtPriceFromInput`).
pub fn get_next_sqrt_price_from_input(
    sqrt_price_x96: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<U256> {
    if sqrt_price_x96.is_zero() || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price_x96, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price_x96, liquidity, amount_in, true)
    }
}

/// Sqrt price after swapping out `amount_out` (`getNextSqrtPriceFromOutput`).
pub fn get_next_sqrt_price_from_output(
    sqrt_price_x96: U256,
    liquidity: u128,
    amount_out: U256,
    zero_for_one: bool,
) -> Option<U256> {
    if sqrt_price_x96.is_zero() || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price_x96, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price_x96, liquidity, amount_out, false)
    }
}

/// token0 needed to move between two sqrt prices at `liquidity`
/// (`getAmount0Delta`).
pub fn get_amount0_delta(
    mut sqrt_a: U256,
    mut sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Option<U256> {
    if sqrt_a > sqrt_b {
        std::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    if sqrt_a.is_zero() {
        return None;
    }
    let numerator1 = U256::from(liquidity) << 96usize;
    let numerator2 = sqrt_b - sqrt_a;
    if round_up {
        div_rounding_up(mul_div_rounding_up(numerator1, numerator2, sqrt_b)?, sqrt_a)
    } else {
        Some(mul_div(numerator1, numerator2, sqrt_b)? / sqrt_a)
    }
}

/// token1 needed to move between two sqrt prices at `liquidity`
/// (`getAmount1Delta`).
pub fn get_amount1_delta(
    mut sqrt_a: U256,
    mut sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Option<U256> {
    if sqrt_a > sqrt_b {
        std::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    let liquidity = U256::from(liquidity);
    if round_up {
        mul_div_rounding_up(liquidity, sqrt_b - sqrt_a, Q96)
    } else {
        mul_div(liquidity, sqrt_b - sqrt_a, Q96)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::uint;

    const ONE_E18: u128 = 1_000_000_000_000_000_000;
    /// `encodePriceSqrt(121, 100)`: sqrt(1.21) * 2^96 = 1.1 * 2^96, floored.
    const PRICE_121_100: U256 = uint!(87150978765690771352898345369_U256);

    // Expected values below are from Uniswap v3-core's SqrtPriceMath tests.

    #[test]
    fn input_of_point_one_token1() {
        let next = get_next_sqrt_price_from_input(Q96, ONE_E18, U256::from(ONE_E18 / 10), false);
        assert_eq!(next, Some(PRICE_121_100));
    }

    #[test]
    fn input_of_point_one_token0() {
        let next = get_next_sqrt_price_from_input(Q96, ONE_E18, U256::from(ONE_E18 / 10), true);
        assert_eq!(next, Some(uint!(72025602285694852357767227579_U256)));
    }

    #[test]
    fn zero_input_returns_same_price() {
        assert_eq!(
            get_next_sqrt_price_from_input(Q96, ONE_E18, U256::ZERO, true),
            Some(Q96)
        );
        assert_eq!(
            get_next_sqrt_price_from_input(Q96, ONE_E18, U256::ZERO, false),
            Some(Q96)
        );
    }

    #[test]
    fn zero_liquidity_or_price_is_rejected() {
        assert_eq!(
            get_next_sqrt_price_from_input(Q96, 0, U256::from(1), true),
            None
        );
        assert_eq!(
            get_next_sqrt_price_from_input(U256::ZERO, 1, U256::from(1), true),
            None
        );
    }

    #[test]
    fn output_cannot_exceed_reserves() {
        // Taking out more token0 than virtual reserves holds must fail.
        assert_eq!(
            get_next_sqrt_price_from_output(Q96, 1, U256::from(4), false),
            None
        );
    }

    #[test]
    fn amount0_delta_between_1_and_121_over_100() {
        assert_eq!(
            get_amount0_delta(Q96, PRICE_121_100, ONE_E18, true),
            Some(U256::from(90_909_090_909_090_910u64))
        );
        assert_eq!(
            get_amount0_delta(Q96, PRICE_121_100, ONE_E18, false),
            Some(U256::from(90_909_090_909_090_909u64))
        );
    }

    #[test]
    fn amount1_delta_between_1_and_121_over_100() {
        assert_eq!(
            get_amount1_delta(Q96, PRICE_121_100, ONE_E18, true),
            Some(U256::from(100_000_000_000_000_000u64))
        );
        assert_eq!(
            get_amount1_delta(Q96, PRICE_121_100, ONE_E18, false),
            Some(U256::from(99_999_999_999_999_999u64))
        );
    }

    #[test]
    fn deltas_are_symmetric_in_argument_order() {
        assert_eq!(
            get_amount0_delta(PRICE_121_100, Q96, ONE_E18, true),
            get_amount0_delta(Q96, PRICE_121_100, ONE_E18, true)
        );
        assert_eq!(
            get_amount1_delta(PRICE_121_100, Q96, ONE_E18, false),
            get_amount1_delta(Q96, PRICE_121_100, ONE_E18, false)
        );
    }
}
