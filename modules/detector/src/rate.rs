//! Marginal (spot) exchange rates, for screening cycles before exact sizing.

use alloy::primitives::{Address, U256};
use kairos_core::FEE_DENOMINATOR;
use kairos_dex::{AnyPool, Pool};

/// `x` as an `f64`, rounded. Exact up to 2^53.
pub(crate) fn u256_to_f64(x: U256) -> f64 {
    x.as_limbs().iter().rev().fold(0.0, |acc, &limb| {
        acc * 18_446_744_073_709_551_616.0 + limb as f64
    })
}

/// `x` as a `U256`, truncated. `None` if negative, not finite, or ≥ 2^256.
pub(crate) fn f64_to_u256(x: f64) -> Option<U256> {
    if !x.is_finite() || x < 0.0 {
        return None;
    }
    if x < 1.0 {
        return Some(U256::ZERO);
    }
    // x = m * 2^e with m a 53-bit integer.
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1075;
    let mantissa = (bits & ((1 << 52) - 1)) | (1 << 52);
    let m = U256::from(mantissa);
    if exponent >= 0 {
        let e = exponent as usize;
        (e + 53 <= 256).then(|| m << e)
    } else {
        Some(m >> (-exponent) as usize)
    }
}

/// Output per unit input for an infinitesimal swap from `token_in` to
/// `token_out`, after the fee. `None` if the pool does not trade the pair or
/// has no liquidity.
pub(crate) fn spot_rate(pool: &AnyPool, token_in: Address, token_out: Address) -> Option<f64> {
    let tokens = pool.tokens();
    let zero_for_one = match (tokens.first(), tokens.get(1)) {
        (Some(&t0), Some(&t1)) if t0 == token_in && t1 == token_out => true,
        (Some(&t0), Some(&t1)) if t1 == token_in && t0 == token_out => false,
        _ => return None,
    };
    let after_fee = |fee: u32| 1.0 - f64::from(fee) / f64::from(FEE_DENOMINATOR);
    let rate = match pool {
        AnyPool::ConstantProduct(p) => {
            let (r0, r1) = p.reserves();
            if r0.is_zero() || r1.is_zero() {
                return None;
            }
            let (r_in, r_out) = if zero_for_one { (r0, r1) } else { (r1, r0) };
            u256_to_f64(r_out) / u256_to_f64(r_in) * after_fee(p.fee_ppm())
        }
        AnyPool::ConcentratedLiquidity(p) => {
            let (sqrt_price, _, liquidity) = p.slot0();
            if sqrt_price.is_zero() || liquidity == 0 {
                return None;
            }
            let sqrt = u256_to_f64(sqrt_price) / 2f64.powi(96);
            let price = sqrt * sqrt; // token1 per token0
            let raw = if zero_for_one { price } else { 1.0 / price };
            raw * after_fee(p.fee_pips())
        }
    };
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_core::ProtocolId;
    use kairos_dex::{ConcentratedLiquidityPool, ConstantProductPool};

    const A: Address = Address::repeat_byte(0x0a);
    const B: Address = Address::repeat_byte(0x0b);

    #[test]
    fn u256_f64_round_trip() {
        for x in [0u128, 1, 12345, 1 << 60, u128::MAX >> 11] {
            let back = f64_to_u256(u256_to_f64(U256::from(x))).unwrap();
            let x = U256::from(x);
            let diff = if back > x { back - x } else { x - back };
            assert!(diff <= x >> 52, "{x} -> {back}");
        }
        assert_eq!(u256_to_f64(U256::from(1) << 200), 2f64.powi(200));
        assert_eq!(f64_to_u256(2f64.powi(200)), Some(U256::from(1) << 200));
        assert_eq!(f64_to_u256(0.7), Some(U256::ZERO));
        assert_eq!(f64_to_u256(-1.0), None);
        assert_eq!(f64_to_u256(2f64.powi(256)), None);
    }

    #[test]
    fn constant_product_rate_includes_fee() {
        let mut p = ConstantProductPool::new(Address::ZERO, ProtocolId(1), A, B, 3000).unwrap();
        p.set_reserves(U256::from(1000), U256::from(2000));
        let p = AnyPool::from(p);
        assert!((spot_rate(&p, A, B).unwrap() - 2.0 * 0.997).abs() < 1e-12);
        assert!((spot_rate(&p, B, A).unwrap() - 0.5 * 0.997).abs() < 1e-12);
        assert_eq!(spot_rate(&p, A, A), None);
    }

    #[test]
    fn concentrated_rate_from_sqrt_price() {
        let mut p =
            ConcentratedLiquidityPool::new(Address::ZERO, ProtocolId(1), A, B, 500, 10).unwrap();
        // sqrt price 2 -> price 4 token1 per token0.
        p.set_slot0(U256::from(2) << 96, 13_863, 1);
        let p = AnyPool::from(p);
        assert!((spot_rate(&p, A, B).unwrap() - 4.0 * 0.9995).abs() < 1e-9);
        assert!((spot_rate(&p, B, A).unwrap() - 0.25 * 0.9995).abs() < 1e-9);
    }
}
