//! 512-bit intermediate multiplication and division (`FullMath.sol`,
//! `UnsafeMath.sol`).

use alloy::primitives::{U256, U512};

/// Narrows a 512-bit value to 256 bits, or `None` if it does not fit.
fn narrow(value: U512) -> Option<U256> {
    let limbs = value.as_limbs();
    limbs[4..]
        .iter()
        .all(|&l| l == 0)
        .then(|| U256::from_limbs_slice(&limbs[..4]))
}

/// `floor(a * b / denominator)` with a full 512-bit intermediate product.
///
/// Returns `None` if `denominator` is zero or the result does not fit in 256
/// bits (where `FullMath.mulDiv` reverts).
pub fn mul_div(a: U256, b: U256, denominator: U256) -> Option<U256> {
    if denominator.is_zero() {
        return None;
    }
    let quotient = U512::from(a) * U512::from(b) / U512::from(denominator);
    narrow(quotient)
}

/// `ceil(a * b / denominator)` with a full 512-bit intermediate product.
///
/// Returns `None` under the same conditions as [`mul_div`], or if rounding up
/// overflows (where `FullMath.mulDivRoundingUp` reverts).
pub fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> Option<U256> {
    if denominator.is_zero() {
        return None;
    }
    let product = U512::from(a) * U512::from(b);
    let denominator = U512::from(denominator);
    let quotient = narrow(product / denominator)?;
    if (product % denominator).is_zero() {
        Some(quotient)
    } else {
        quotient.checked_add(U256::from(1))
    }
}

/// `ceil(x / y)` (`UnsafeMath.divRoundingUp`). Returns `None` if `y` is zero.
pub fn div_rounding_up(x: U256, y: U256) -> Option<U256> {
    if y.is_zero() {
        return None;
    }
    let quotient = x / y;
    Some(if (x % y).is_zero() {
        quotient
    } else {
        quotient + U256::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_uses_full_width_intermediate() {
        // MAX * MAX / MAX overflows 256 bits in the middle but not at the end.
        assert_eq!(mul_div(U256::MAX, U256::MAX, U256::MAX), Some(U256::MAX));
    }

    #[test]
    fn mul_div_rejects_zero_denominator_and_overflow() {
        assert_eq!(mul_div(U256::from(1), U256::from(1), U256::ZERO), None);
        assert_eq!(mul_div(U256::MAX, U256::from(2), U256::from(1)), None);
    }

    #[test]
    fn rounding_up_only_when_remainder() {
        let seven = U256::from(7);
        let two = U256::from(2);
        assert_eq!(mul_div(seven, U256::from(1), two), Some(U256::from(3)));
        assert_eq!(
            mul_div_rounding_up(seven, U256::from(1), two),
            Some(U256::from(4))
        );
        assert_eq!(
            mul_div_rounding_up(U256::from(8), U256::from(1), two),
            Some(U256::from(4))
        );
        assert_eq!(div_rounding_up(seven, two), Some(U256::from(4)));
        assert_eq!(div_rounding_up(U256::from(8), two), Some(U256::from(4)));
    }

    #[test]
    fn rounding_up_overflow_is_none() {
        // MAX * 1 / 1 == MAX exactly: fine. (MAX * MAX) / (MAX - 1) rounds past MAX.
        assert_eq!(
            mul_div_rounding_up(U256::MAX, U256::from(1), U256::from(1)),
            Some(U256::MAX)
        );
        assert_eq!(
            mul_div_rounding_up(U256::MAX, U256::MAX, U256::MAX - U256::from(1)),
            None
        );
    }
}
