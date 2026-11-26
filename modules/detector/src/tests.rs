use alloy::primitives::{Address, U256};
use kairos_core::{ChainProfile, GasEstimates, PoolTier, ProtocolId};
use kairos_dex::math::tick_math::get_sqrt_ratio_at_tick;
use kairos_dex::{AnyPool, ConcentratedLiquidityPool, ConstantProductPool, TickLiquidity};
use kairos_state::StateSnapshot;

use super::*;

// Sorted so USDC < DAI < WETH, matching token0/token1 order.
const USDC: Address = Address::repeat_byte(0x0a);
const DAI: Address = Address::repeat_byte(0x0b);
const WETH: Address = Address::repeat_byte(0x0c);

const E18: u128 = 1_000_000_000_000_000_000;
const E6: u128 = 1_000_000;

fn profile() -> ChainProfile {
    ChainProfile {
        name: "test".into(),
        chain_id: 1,
        multicall3: Address::ZERO,
        wrapped_native: WETH,
        gas: GasEstimates {
            tx_base: 21_000,
            flashloan: 80_000,
            executor_overhead: 30_000,
            per_initialized_tick: 5_000,
            constant_product: Some(60_000),
            concentrated_liquidity: Some(180_000),
            stable_swap: None,
            weighted_pool: None,
            liquidity_book: None,
        },
    }
}

fn detector(config: DetectorConfig) -> Detector {
    Detector::new(config, &profile())
}

/// Constant-product pool with `r0` of the lower token and `r1` of the higher.
fn cp(byte: u8, a: Address, b: Address, r0: u128, r1: u128) -> AnyPool {
    let (t0, t1) = if a < b { (a, b) } else { (b, a) };
    let mut p =
        ConstantProductPool::new(Address::repeat_byte(byte), ProtocolId(1), t0, t1, 3000).unwrap();
    p.set_reserves(U256::from(r0), U256::from(r1));
    p.into()
}

/// USDC/WETH with 1000 WETH at `usdc_per_weth`.
fn usdc_weth(byte: u8, usdc_per_weth: u128) -> AnyPool {
    cp(byte, USDC, WETH, 1000 * usdc_per_weth * E6, 1000 * E18)
}

/// Full-range USDC/WETH concentrated pool near 2000 USDC per WETH (tick
/// 200_311), about 1000 WETH deep, 0.05% fee.
fn usdc_weth_cl(byte: u8) -> AnyPool {
    let mut p = ConcentratedLiquidityPool::new(
        Address::repeat_byte(byte),
        ProtocolId(2),
        USDC,
        WETH,
        500,
        10,
    )
    .unwrap();
    let tick = 200_311;
    let liquidity: u128 = 45_000_000_000_000_000;
    p.set_slot0(get_sqrt_ratio_at_tick(tick).unwrap(), tick, liquidity);
    let net = liquidity as i128;
    p.set_ticks(
        [
            (
                -887_270,
                TickLiquidity {
                    gross: liquidity,
                    net,
                },
            ),
            (
                887_270,
                TickLiquidity {
                    gross: liquidity,
                    net: -net,
                },
            ),
        ],
        kairos_dex::math::tick_math::MIN_TICK..=kairos_dex::math::tick_math::MAX_TICK,
    )
    .unwrap();
    p.into()
}

fn snapshot(pools: Vec<AnyPool>) -> StateSnapshot {
    StateSnapshot::from_pools(1, 100, pools.into_iter().map(|p| (p, PoolTier::Hot)))
}

fn all(snap: &StateSnapshot) -> Vec<Address> {
    snap.changed_since(0).collect()
}

fn gross_in_start_token(o: &ArbOpportunity) -> U256 {
    o.hops.last().unwrap().expected_out - o.hops[0].amount_in
}

#[test]
fn finds_two_pool_arb_on_one_pair() {
    let snap = snapshot(vec![usdc_weth(1, 2000), usdc_weth(2, 2040)]);
    let found = detector(DetectorConfig::default()).detect(&snap, &all(&snap), 0);
    assert_eq!(found.len(), 1, "{found:#?}");
    let o = &found[0];

    // Sell WETH where it is dear (pool 2), buy it back where it is cheap.
    assert_eq!(o.hops.len(), 2);
    assert_eq!(o.hops[0].pool_address, Address::repeat_byte(2));
    assert_eq!((o.hops[0].token_in, o.hops[0].token_out), (WETH, USDC));
    assert_eq!(o.hops[1].amount_in, o.hops[0].expected_out);
    assert_eq!(o.total_gas, 131_000 + 2 * 60_000);
    assert_eq!(o.total_profit_wei, gross_in_start_token(o));
    assert_eq!(o.net_profit_wei, o.total_profit_wei);
    assert_eq!(o.block_number, 100);
}

#[test]
fn chosen_size_is_the_profit_maximum() {
    let snap = snapshot(vec![usdc_weth(1, 2000), usdc_weth(2, 2040)]);
    let o = &detector(DetectorConfig::default()).detect(&snap, &all(&snap), 0)[0];
    let cycle: Vec<Hop> = o
        .hops
        .iter()
        .map(|h| Hop {
            pool: h.pool_address,
            token_in: h.token_in,
            token_out: h.token_out,
        })
        .collect();
    let x = o.hops[0].amount_in;
    let g = |x: U256| sizing::quote(&snap, &cycle, x).map(|a| a[2].saturating_sub(x));
    let best = g(x).unwrap();
    for delta in [U256::from(1), x / U256::from(1000), x / U256::from(10)] {
        assert!(g(x + delta).unwrap() <= best);
        assert!(g(x - delta).unwrap() <= best);
    }
}

#[test]
fn no_arb_when_spread_is_inside_fees() {
    // 0.3% + 0.3% fees exceed a 0.5% spread.
    let snap = snapshot(vec![usdc_weth(1, 2000), usdc_weth(2, 2010)]);
    assert!(
        detector(DetectorConfig::default())
            .detect(&snap, &all(&snap), 0)
            .is_empty()
    );
}

#[test]
fn gas_price_and_min_profit_filter() {
    let snap = snapshot(vec![usdc_weth(1, 2000), usdc_weth(2, 2040)]);
    let d = detector(DetectorConfig::default());
    let gross = d.detect(&snap, &all(&snap), 0)[0].total_profit_wei;

    // Gas that costs more than the gross profit.
    let gas_price = (gross / U256::from(251_000u64) + U256::from(1)).to::<u128>();
    assert!(d.detect(&snap, &all(&snap), gas_price).is_empty());

    let d = detector(DetectorConfig {
        min_profit_wei: gross + U256::from(1),
        ..Default::default()
    });
    assert!(d.detect(&snap, &all(&snap), 0).is_empty());
}

#[test]
fn only_cycles_through_changed_pools() {
    let snap = snapshot(vec![
        usdc_weth(1, 2000),
        usdc_weth(2, 2040),
        usdc_weth(3, 2000),
    ]);
    let d = detector(DetectorConfig::default());
    assert!(d.detect(&snap, &[], 0).is_empty());

    // Only pool 1 changed: the 1<>2 cycle, not the equally good 3<>2 one.
    let found = d.detect(&snap, &[Address::repeat_byte(1)], 0);
    assert_eq!(found.len(), 1);
    assert!(
        found[0]
            .hops
            .iter()
            .any(|h| h.pool_address == Address::repeat_byte(1))
    );

    // All changed: both cycles through the mispriced pool, best first.
    let found = d.detect(&snap, &all(&snap), 0);
    assert_eq!(found.len(), 2);
    assert!(found[0].net_profit_wei >= found[1].net_profit_wei);
}

#[test]
fn concentrated_and_constant_product_arb_counts_tick_gas() {
    // The concentrated pool sits near 2000; the constant-product pool at 2060.
    let snap = snapshot(vec![usdc_weth_cl(1), usdc_weth(2, 2060)]);
    let found = detector(DetectorConfig::default()).detect(&snap, &all(&snap), 0);
    assert_eq!(found.len(), 1, "{found:#?}");
    let o = &found[0];
    let cl_hop = o
        .hops
        .iter()
        .find(|h| h.pool_address == Address::repeat_byte(1))
        .unwrap();
    assert!(cl_hop.estimated_gas >= 180_000);
    assert!(o.net_profit_wei > U256::ZERO);
}

#[test]
fn non_native_start_token_is_valued_in_wei() {
    let snap = snapshot(vec![usdc_weth(1, 2000), usdc_weth(2, 2040)]);
    let found = detector(DetectorConfig {
        start_tokens: vec![USDC],
        ..Default::default()
    })
    .detect(&snap, &all(&snap), 0);
    assert_eq!(found.len(), 1);
    let o = &found[0];
    assert_eq!(o.hops[0].token_in, USDC);
    // Profit in USDC (6 decimals) converted at ~2000-2040 USDC per WETH.
    let usdc = gross_in_start_token(o);
    let wei = o.total_profit_wei;
    let implied = u256_to_f64(usdc) * 1e12 / u256_to_f64(wei);
    assert!((1990.0..2050.0).contains(&implied), "{implied}");
}

#[test]
fn triangle_needs_three_hops() {
    // WETH -> USDC at 2000, USDC -> DAI 1:1, DAI -> WETH at 1900: buying
    // WETH with DAI is cheap, so WETH -> USDC -> DAI -> WETH pays.
    let pools = vec![
        usdc_weth(1, 2000),
        cp(2, USDC, DAI, 10_000_000 * E6, 10_000_000 * E18),
        cp(3, DAI, WETH, 1900 * 1000 * E18, 1000 * E18),
    ];
    let snap = snapshot(pools);
    let two = detector(DetectorConfig {
        max_hops: 2,
        ..Default::default()
    });
    assert!(two.detect(&snap, &all(&snap), 0).is_empty());

    let found = detector(DetectorConfig::default()).detect(&snap, &all(&snap), 0);
    assert_eq!(found.len(), 1, "{found:#?}");
    let path: Vec<_> = found[0].hops.iter().map(|h| h.token_in).collect();
    assert_eq!(path, vec![WETH, USDC, DAI]);
}
