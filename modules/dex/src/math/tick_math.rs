//! Conversion between ticks and `sqrtPriceX96` (`TickMath.sol`).
//!
//! A tick `t` corresponds to price `1.0001^t` (token1 per token0), stored on
//! chain as `sqrt(price) * 2^96`.

use alloy::primitives::{U256, uint};

/// Lowest tick a pool can reach (`TickMath.MIN_TICK`).
pub const MIN_TICK: i32 = -887_272;
/// Highest tick a pool can reach (`TickMath.MAX_TICK`).
pub const MAX_TICK: i32 = 887_272;

/// `get_sqrt_ratio_at_tick(MIN_TICK)` (`TickMath.MIN_SQRT_RATIO`).
pub const MIN_SQRT_RATIO: U256 = uint!(4295128739_U256);
/// `get_sqrt_ratio_at_tick(MAX_TICK)` (`TickMath.MAX_SQRT_RATIO`).
pub const MAX_SQRT_RATIO: U256 = uint!(1461446703485210103287273052203988822378723970342_U256);

/// `sqrt(1.0001^-(2^i))` in Q128, for bits `i = 1..=19` of `|tick|`.
const BIT_MULTIPLIERS: [(u32, U256); 19] = [
    (0x2, uint!(0xfff97272373d413259a46990580e213a_U256)),
    (0x4, uint!(0xfff2e50f5f656932ef12357cf3c7fdcc_U256)),
    (0x8, uint!(0xffe5caca7e10e4e61c3624eaa0941cd0_U256)),
    (0x10, uint!(0xffcb9843d60f6159c9db58835c926644_U256)),
    (0x20, uint!(0xff973b41fa98c081472e6896dfb254c0_U256)),
    (0x40, uint!(0xff2ea16466c96a3843ec78b326b52861_U256)),
    (0x80, uint!(0xfe5dee046a99a2a811c461f1969c3053_U256)),
    (0x100, uint!(0xfcbe86c7900a88aedcffc83b479aa3a4_U256)),
    (0x200, uint!(0xf987a7253ac413176f2b074cf7815e54_U256)),
    (0x400, uint!(0xf3392b0822b70005940c7a398e4b70f3_U256)),
    (0x800, uint!(0xe7159475a2c29b7443b29c7fa6e889d9_U256)),
    (0x1000, uint!(0xd097f3bdfd2022b8845ad8f792aa5825_U256)),
    (0x2000, uint!(0xa9f746462d870fdf8a65dc1f90e061e5_U256)),
    (0x4000, uint!(0x70d869a156d2a1b890bb3df62baf32f7_U256)),
    (0x8000, uint!(0x31be135f97d08fd981231505542fcfa6_U256)),
    (0x10000, uint!(0x9aa508b5b7a84e1c677de54f3e99bc9_U256)),
    (0x20000, uint!(0x5d6af8dedb81196699c329225ee604_U256)),
    (0x40000, uint!(0x2216e584f5fa1ea926041bedfe98_U256)),
    (0x80000, uint!(0x48a170391f7dc42444e8fa2_U256)),
];

/// `sqrt(1.0001^tick) * 2^96`, rounded up (`TickMath.getSqrtRatioAtTick`).
///
/// Returns `None` if `tick` is outside `[MIN_TICK, MAX_TICK]`.
pub fn get_sqrt_ratio_at_tick(tick: i32) -> Option<U256> {
    let abs_tick = tick.unsigned_abs();
    if abs_tick > MAX_TICK as u32 {
        return None;
    }

    let mut ratio = if abs_tick & 0x1 != 0 {
        uint!(0xfffcb933bd6fad37aa2d162d1a594001_U256)
    } else {
        uint!(0x100000000000000000000000000000000_U256)
    };
    for (bit, multiplier) in BIT_MULTIPLIERS {
        if abs_tick & bit != 0 {
            ratio = (ratio * multiplier) >> 128;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    // Q128 -> Q96, rounding up so the result is always >= the true value.
    let rounded_up = !(ratio & U256::from(u32::MAX)).is_zero();
    Some((ratio >> 32) + U256::from(rounded_up as u8))
}

/// Greatest tick whose sqrt ratio is `<= sqrt_price_x96`
/// (`TickMath.getTickAtSqrtRatio`).
///
/// Returns `None` if `sqrt_price_x96` is outside
/// `[MIN_SQRT_RATIO, MAX_SQRT_RATIO)`.
///
/// Implemented as a binary search over [`get_sqrt_ratio_at_tick`] rather than
/// porting the Solidity log2 approximation. Both return the greatest tick
/// satisfying the definition above, which is the only property the swap loop
/// relies on; this one is exact by construction.
pub fn get_tick_at_sqrt_ratio(sqrt_price_x96: U256) -> Option<i32> {
    if sqrt_price_x96 < MIN_SQRT_RATIO || sqrt_price_x96 >= MAX_SQRT_RATIO {
        return None;
    }
    // Invariant: ratio(lo) <= sqrt_price_x96 < ratio(hi).
    let (mut lo, mut hi) = (MIN_TICK, MAX_TICK);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if get_sqrt_ratio_at_tick(mid)? <= sqrt_price_x96 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `floor(sqrt(1.0001^t) * 2^96)` computed with 120-digit decimals. One
    /// entry per bit of `|tick|`, both signs, so every multiplier is exercised.
    const EXACT: &[(i32, &str)] = &[
        (0, "79228162514264337593543950336"),
        (1, "79232123823359799118286999567"),
        (-1, "79224201403219477170569942573"),
        (2, "79236085330515764027303304731"),
        (-2, "79220240490215316061937756560"),
        (4, "79244008939048815603706035061"),
        (-4, "79212319258289487113226433916"),
        (8, "79259858533276714757314932305"),
        (-8, "79196479170490597288862688490"),
        (16, "79291567232598584799939703904"),
        (-16, "79164808496886665658930780291"),
        (32, "79355022692464371645785046466"),
        (-32, "79101505139923049997807806614"),
        (64, "79482085999252804386437311141"),
        (-64, "78975050245229982702767995059"),
        (128, "79736823300114093921829183326"),
        (-128, "78722746600537056721934508529"),
        (256, "80248749790819932309965073892"),
        (-256, "78220554859095770638340573243"),
        (512, "81282483887344747381513967011"),
        (-512, "77225761753129597550065289036"),
        (1024, "83390072131320151908154831281"),
        (-1024, "75273969370139069689486932537"),
        (2048, "87770609709833776024991924138"),
        (-2048, "71517125791179246722882903167"),
        (4096, "97234110755111693312479820773"),
        (-4096, "64556580881331167221767657719"),
        (8192, "119332217159966728226237229890"),
        (-8192, "52601903197458624361810746399"),
        (16384, "179736315981702064433883588727"),
        (-16384, "34923947901690145425342545398"),
        (32768, "407748233172238350107850275304"),
        (-32768, "15394552875315951095595078917"),
        (65536, "2098478828474011932436660412517"),
        (-65536, "2991262837734375505310244436"),
        (131072, "55581415166113811149459800483533"),
        (-131072, "112935262922445818024280873"),
        (262144, "38992368544603139932233054999993551"),
        (-262144, "160982827401375763736068"),
        (524288, "19190206568837448476620805521939294697349"),
        (-524288, "327099227039063106"),
        (123456, "37980312163600838849089827163597"),
        (-123456, "165272515621987480497246432"),
    ];

    #[test]
    fn known_on_chain_values_match_exactly() {
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK), Some(MIN_SQRT_RATIO));
        assert_eq!(get_sqrt_ratio_at_tick(MAX_TICK), Some(MAX_SQRT_RATIO));
        assert_eq!(
            get_sqrt_ratio_at_tick(0),
            Some(uint!(79228162514264337593543950336_U256))
        );
        assert_eq!(
            get_sqrt_ratio_at_tick(1),
            Some(uint!(79232123823359799118286999568_U256))
        );
        assert_eq!(
            get_sqrt_ratio_at_tick(-1),
            Some(uint!(79224201403219477170569942574_U256))
        );
    }

    /// Every bit multiplier agrees with the true value to within the
    /// algorithm's own precision (~1e-19 relative at the extremes).
    #[test]
    fn every_bit_multiplier_is_accurate() {
        for &(tick, exact) in EXACT {
            let exact: U256 = exact.parse().unwrap();
            let got = get_sqrt_ratio_at_tick(tick).unwrap();
            let diff = if got > exact {
                got - exact
            } else {
                exact - got
            };
            let tolerance = exact / U256::from(10u64.pow(18)) + U256::from(2);
            assert!(diff <= tolerance, "tick {tick}: got {got}, exact {exact}");
        }
    }

    #[test]
    fn out_of_range_ticks_are_rejected() {
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK - 1), None);
        assert_eq!(get_sqrt_ratio_at_tick(MAX_TICK + 1), None);
    }

    #[test]
    fn tick_at_sqrt_ratio_is_greatest_tick_at_or_below() {
        for tick in [
            MIN_TICK,
            -500_000,
            -60,
            -1,
            0,
            1,
            59,
            60,
            123_456,
            MAX_TICK - 1,
        ] {
            let ratio = get_sqrt_ratio_at_tick(tick).unwrap();
            assert_eq!(get_tick_at_sqrt_ratio(ratio), Some(tick));
            if tick < MAX_TICK - 1 {
                let next = get_sqrt_ratio_at_tick(tick + 1).unwrap();
                assert_eq!(get_tick_at_sqrt_ratio(next - U256::from(1)), Some(tick));
            }
        }
    }

    #[test]
    fn tick_at_sqrt_ratio_bounds() {
        assert_eq!(get_tick_at_sqrt_ratio(MIN_SQRT_RATIO - U256::from(1)), None);
        assert_eq!(get_tick_at_sqrt_ratio(MAX_SQRT_RATIO), None);
        assert_eq!(
            get_tick_at_sqrt_ratio(MAX_SQRT_RATIO - U256::from(1)),
            Some(MAX_TICK - 1)
        );
    }
}
