//! Choosing the input amount that maximizes a cycle's gross profit, using
//! the pools' exact integer quotes.

use alloy::primitives::U256;
use kairos_dex::Pool;
use kairos_state::StateSnapshot;

use crate::cycles::Hop;

/// Amounts along the cycle for input `amount_in`: `[amount_in, out_1, …,
/// out_n]`. `None` if any hop cannot fill.
pub(crate) fn quote(snapshot: &StateSnapshot, hops: &[Hop], amount_in: U256) -> Option<Vec<U256>> {
    let mut amounts = Vec::with_capacity(hops.len() + 1);
    amounts.push(amount_in);
    let mut amount = amount_in;
    for hop in hops {
        amount = snapshot
            .pool(&hop.pool)?
            .get_amount_out(hop.token_in, hop.token_out, amount)?;
        amounts.push(amount);
    }
    Some(amounts)
}

fn gain(snapshot: &StateSnapshot, hops: &[Hop], amount_in: U256) -> U256 {
    quote(snapshot, hops, amount_in)
        .map(|a| a[a.len() - 1].saturating_sub(amount_in))
        .unwrap_or_default()
}

/// Amounts along the cycle at the input that maximizes gross profit, or
/// `None` if no input is profitable.
///
/// Profit as a function of input rises, peaks and falls (each hop's output
/// is concave in its input). A scan over powers of two brackets the peak
/// across the full `U256` range in about one quote per bit, then a ternary
/// search narrows the bracket to the exact integer.
pub(crate) fn optimal(snapshot: &StateSnapshot, hops: &[Hop]) -> Option<Vec<U256>> {
    let mut best: Option<(usize, U256)> = None;
    let mut quoted = false;
    for k in 0..255 {
        let x = U256::from(1) << k;
        // Dust inputs quote to nothing; past the first fill, `None` means
        // liquidity ran out and larger inputs cannot fill either.
        let Some(amounts) = quote(snapshot, hops, x) else {
            if quoted {
                break;
            }
            continue;
        };
        quoted = true;
        let g = amounts[amounts.len() - 1].saturating_sub(x);
        if !g.is_zero() && best.is_none_or(|(_, b)| g > b) {
            best = Some((k, g));
        }
    }
    let (k, _) = best?;

    let mut lo = if k == 0 {
        U256::from(1)
    } else {
        U256::from(1) << (k - 1)
    };
    let mut hi = U256::from(1) << (k + 1);
    while hi - lo > U256::from(2) {
        let third = (hi - lo) / U256::from(3);
        let (m1, m2) = (lo + third, hi - third);
        if gain(snapshot, hops, m1) < gain(snapshot, hops, m2) {
            lo = m1;
        } else {
            hi = m2;
        }
    }
    let mut best_x = lo;
    let mut x = lo;
    while x <= hi {
        if gain(snapshot, hops, x) > gain(snapshot, hops, best_x) {
            best_x = x;
        }
        x += U256::from(1);
    }
    let amounts = quote(snapshot, hops, best_x)?;
    (amounts[amounts.len() - 1] > best_x).then_some(amounts)
}
