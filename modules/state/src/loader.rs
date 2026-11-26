//! Reads pool state from the chain at a fixed block.
//!
//! Every read goes through Multicall3's `aggregate3`, so loading a pool costs
//! a few `eth_call`s regardless of how many fields or ticks it needs:
//!
//! 1. Tokens, reserves or `slot0` / `liquidity` / `fee` / `tickSpacing`, and
//!    for Aerodrome pools `stable` and `factory`.
//! 2. Aerodrome fees from the factory, and the tick-bitmap words around each
//!    concentrated pool's current tick.
//! 3. `ticks()` for every initialized tick found in those words.
//!
//! Return values are decoded leniently: only the leading fields Kairos needs
//! are declared, so venues whose functions return extra trailing fields
//! (Slipstream's `ticks()`, Aerodrome's `getReserves()`) decode with the same
//! ABI as Uniswap's.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use kairos_core::{FEE_DENOMINATOR, KairosError, Result};
use kairos_dex::math::tick_math::{MAX_TICK, MIN_TICK};
use kairos_dex::{AnyPool, ConcentratedLiquidityPool, ConstantProductPool, Pool, TickLiquidity};

use crate::config::{PoolConfig, PoolKind};

mod abi {
    alloy::sol! {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result3 {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result3[] memory returnData);

        function token0() external view returns (address);
        function token1() external view returns (address);
        function factory() external view returns (address);
        function stable() external view returns (bool);
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1);
        function getFee(address pool, bool stable) external view returns (uint256);

        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick);
        function liquidity() external view returns (uint128);
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        function ticks(int24 tick) external view returns (uint128 liquidityGross, int128 liquidityNet);
    }
}

/// Most calls sent in one `aggregate3`, to stay well under node gas and
/// response-size limits for `eth_call`.
const MAX_CALLS_PER_MULTICALL: usize = 400;

/// Aerodrome's `getFee` denominator (basis points).
const AERODROME_FEE_DENOMINATOR: u32 = 10_000;

/// Reads pools through a Multicall3 contract.
#[derive(Debug, Clone)]
pub struct PoolLoader<P> {
    provider: P,
    multicall: Address,
    tick_window_words: u16,
}

/// One call in a multicall batch, decoded by the caller.
pub(crate) struct Call {
    target: Address,
    data: Bytes,
    allow_failure: bool,
}

pub(crate) fn call<C: SolCall>(target: Address, c: C) -> Call {
    Call {
        target,
        data: c.abi_encode().into(),
        allow_failure: false,
    }
}

/// Like [`call`], but a revert does not fail the batch: its slot in the
/// results is `None`.
pub(crate) fn optional_call<C: SolCall>(target: Address, c: C) -> Call {
    Call {
        allow_failure: true,
        ..call(target, c)
    }
}

pub(crate) fn decode<C: SolCall>(target: Address, data: &Bytes) -> Result<C::Return> {
    C::abi_decode_returns(data).map_err(|e| {
        KairosError::NodeConnectionFailed(format!("{target}: cannot decode {}: {e}", C::SIGNATURE))
    })
}

pub(crate) fn rpc_err(context: &str, e: impl std::fmt::Display) -> KairosError {
    KairosError::NodeConnectionFailed(format!("{context}: {e}"))
}

/// Runs `calls` at `block` through the Multicall3 deployment at `multicall`
/// and returns each call's return data, or `None` for an
/// [`optional_call`] that reverted. A required call that fails fails the
/// whole batch.
pub(crate) async fn multicall<P: Provider>(
    provider: &P,
    multicall: Address,
    calls: &[Call],
    block: BlockId,
) -> Result<Vec<Option<Bytes>>> {
    let mut out = Vec::with_capacity(calls.len());
    for chunk in calls.chunks(MAX_CALLS_PER_MULTICALL) {
        let request = abi::aggregate3Call {
            calls: chunk
                .iter()
                .map(|c| abi::Call3 {
                    target: c.target,
                    allowFailure: c.allow_failure,
                    callData: c.data.clone(),
                })
                .collect(),
        };
        let tx = TransactionRequest::default()
            .to(multicall)
            .input(request.abi_encode().into());
        let raw = provider
            .call(tx)
            .block(block)
            .await
            .map_err(|e| rpc_err("multicall", e))?;
        let results = abi::aggregate3Call::abi_decode_returns(&raw)
            .map_err(|e| rpc_err("multicall: bad response", e))?;
        if results.len() != chunk.len() {
            return Err(rpc_err(
                "multicall",
                format!("{} results for {} calls", results.len(), chunk.len()),
            ));
        }
        out.extend(
            results
                .into_iter()
                .map(|r| r.success.then_some(r.returnData)),
        );
    }
    Ok(out)
}

/// Range of tick-bitmap words to load around `tick`, and the range of ticks
/// those words fully describe.
pub(crate) fn tick_window(
    tick: i32,
    tick_spacing: i32,
    words_each_side: u16,
) -> (RangeInclusive<i16>, RangeInclusive<i32>) {
    let word_of = |t: i32| t.div_euclid(tick_spacing).div_euclid(256);
    let min_word = word_of(MIN_TICK);
    let max_word = word_of(MAX_TICK);
    let center = word_of(tick.clamp(MIN_TICK, MAX_TICK));
    let k = i32::from(words_each_side);
    let lo = (center - k).max(min_word);
    let hi = (center + k).min(max_word);
    let coverage_lo = (lo * 256 * tick_spacing).max(MIN_TICK);
    let coverage_hi = ((hi * 256 + 255) * tick_spacing).min(MAX_TICK);
    // Words of valid ticks always fit in int16: |MIN_TICK| / 256 < 2^15.
    (lo as i16..=hi as i16, coverage_lo..=coverage_hi)
}

impl<P: Provider + Clone> PoolLoader<P> {
    /// Creates a loader that batches reads through the Multicall3 deployment
    /// at `multicall`.
    pub fn new(provider: P, multicall: Address, tick_window_words: u16) -> Self {
        Self {
            provider,
            multicall,
            tick_window_words,
        }
    }

    /// The node's latest block number.
    pub async fn block_number(&self) -> Result<u64> {
        self.provider
            .get_block_number()
            .await
            .map_err(|e| rpc_err("eth_blockNumber", e))
    }

    async fn multicall(&self, calls: &[Call], block: BlockId) -> Result<Vec<Bytes>> {
        let results = multicall(&self.provider, self.multicall, calls, block).await?;
        // Every call the loader makes is required, so none is `None`.
        results
            .into_iter()
            .map(|r| r.ok_or_else(|| KairosError::Internal("required call failed".into())))
            .collect()
    }

    /// Loads every pool in `configs` as of `block`.
    ///
    /// # Errors
    ///
    /// [`KairosError::NodeConnectionFailed`] if an RPC call fails or returns
    /// data that does not decode; [`KairosError::ConfigError`] if a pool is
    /// not what its config says (an Aerodrome stable pool, unsorted tokens,
    /// an invalid fee).
    pub async fn load(&self, configs: &[PoolConfig], block: u64) -> Result<Vec<AnyPool>> {
        let at = BlockId::number(block);

        // Round 1: per-pool fields.
        let mut calls = Vec::new();
        for c in configs {
            let a = c.address;
            calls.push(call(a, abi::token0Call {}));
            calls.push(call(a, abi::token1Call {}));
            match c.kind {
                PoolKind::UniswapV2 => calls.push(call(a, abi::getReservesCall {})),
                PoolKind::AerodromeV2 => {
                    calls.push(call(a, abi::getReservesCall {}));
                    calls.push(call(a, abi::stableCall {}));
                    calls.push(call(a, abi::factoryCall {}));
                }
                PoolKind::UniswapV3 | PoolKind::Slipstream => {
                    calls.push(call(a, abi::feeCall {}));
                    calls.push(call(a, abi::tickSpacingCall {}));
                    calls.push(call(a, abi::slot0Call {}));
                    calls.push(call(a, abi::liquidityCall {}));
                }
            }
        }
        let results = self.multicall(&calls, at).await?;
        let mut results = results.iter();
        let mut next = || {
            results
                .next()
                .ok_or_else(|| KairosError::Internal("multicall result count".into()))
        };

        enum Partial {
            Ready(AnyPool),
            Aerodrome(ConstantProductPool, Address),
            Concentrated(ConcentratedLiquidityPool),
        }
        let mut partials = Vec::with_capacity(configs.len());
        for c in configs {
            let a = c.address;
            let token0 = decode::<abi::token0Call>(a, next()?)?;
            let token1 = decode::<abi::token1Call>(a, next()?)?;
            let partial = match c.kind {
                PoolKind::UniswapV2 => {
                    let r = decode::<abi::getReservesCall>(a, next()?)?;
                    let fee = c.fee_ppm.ok_or_else(|| {
                        KairosError::ConfigError(format!("pool {a}: missing fee_ppm"))
                    })?;
                    let mut pool = ConstantProductPool::new(a, c.protocol_id, token0, token1, fee)?;
                    pool.set_reserves(r.reserve0, r.reserve1);
                    Partial::Ready(pool.into())
                }
                PoolKind::AerodromeV2 => {
                    let r = decode::<abi::getReservesCall>(a, next()?)?;
                    let stable = decode::<abi::stableCall>(a, next()?)?;
                    let factory = decode::<abi::factoryCall>(a, next()?)?;
                    if stable {
                        return Err(KairosError::ConfigError(format!(
                            "pool {a}: Aerodrome stable pools are not supported"
                        )));
                    }
                    // Placeholder fee until round 2 reads the real one.
                    let mut pool = ConstantProductPool::new(a, c.protocol_id, token0, token1, 0)?;
                    pool.set_reserves(r.reserve0, r.reserve1);
                    Partial::Aerodrome(pool, factory)
                }
                PoolKind::UniswapV3 | PoolKind::Slipstream => {
                    let fee = decode::<abi::feeCall>(a, next()?)?;
                    let spacing = decode::<abi::tickSpacingCall>(a, next()?)?;
                    let slot0 = decode::<abi::slot0Call>(a, next()?)?;
                    let liquidity = decode::<abi::liquidityCall>(a, next()?)?;
                    let mut pool = ConcentratedLiquidityPool::new(
                        a,
                        c.protocol_id,
                        token0,
                        token1,
                        fee.to::<u32>(),
                        spacing.as_i32(),
                    )?;
                    pool.set_slot0(
                        U256::from(slot0.sqrtPriceX96),
                        slot0.tick.as_i32(),
                        liquidity,
                    );
                    Partial::Concentrated(pool)
                }
            };
            partials.push(partial);
        }

        // Round 2: Aerodrome fees and tick-bitmap words.
        let mut calls = Vec::new();
        let mut windows = HashMap::new();
        for p in &partials {
            match p {
                Partial::Aerodrome(pool, factory) => calls.push(call(
                    *factory,
                    abi::getFeeCall {
                        pool: pool.id().address,
                        stable: false,
                    },
                )),
                Partial::Concentrated(pool) => {
                    let (words, coverage) =
                        tick_window(pool.slot0().1, pool.tick_spacing(), self.tick_window_words);
                    for w in words.clone() {
                        calls.push(call(
                            pool.id().address,
                            abi::tickBitmapCall { wordPosition: w },
                        ));
                    }
                    windows.insert(pool.id().address, (words, coverage));
                }
                Partial::Ready(_) => {}
            }
        }
        let results = self.multicall(&calls, at).await?;
        let mut results = results.iter();

        let mut tick_calls = Vec::new();
        let mut tick_owners = Vec::new();
        for p in &mut partials {
            match p {
                Partial::Aerodrome(pool, _) => {
                    let a = pool.id().address;
                    let bps = decode::<abi::getFeeCall>(a, next_of(&mut results)?)?;
                    let fee_ppm = u32::try_from(bps)
                        .ok()
                        .filter(|b| *b < AERODROME_FEE_DENOMINATOR)
                        .map(|b| b * (FEE_DENOMINATOR / AERODROME_FEE_DENOMINATOR))
                        .ok_or_else(|| {
                            KairosError::ConfigError(format!("pool {a}: invalid fee {bps} bps"))
                        })?;
                    pool.set_fee_ppm(fee_ppm)?;
                }
                Partial::Concentrated(pool) => {
                    let a = pool.id().address;
                    let spacing = pool.tick_spacing();
                    let (words, _) = &windows[&a];
                    for w in words.clone() {
                        let bitmap = decode::<abi::tickBitmapCall>(a, next_of(&mut results)?)?;
                        for bit in 0..256 {
                            if bitmap.bit(bit) {
                                let tick = (i32::from(w) * 256 + bit as i32) * spacing;
                                tick_calls.push(call(
                                    a,
                                    abi::ticksCall {
                                        tick: tick.try_into().map_err(|_| {
                                            KairosError::Internal(format!("tick {tick} overflows"))
                                        })?,
                                    },
                                ));
                                tick_owners.push((a, tick));
                            }
                        }
                    }
                }
                Partial::Ready(_) => {}
            }
        }

        // Round 3: liquidity at every initialized tick.
        let results = self.multicall(&tick_calls, at).await?;
        let mut ticks: HashMap<Address, Vec<(i32, TickLiquidity)>> = HashMap::new();
        for ((pool, tick), data) in tick_owners.into_iter().zip(&results) {
            let t = decode::<abi::ticksCall>(pool, data)?;
            if t.liquidityGross > 0 {
                ticks.entry(pool).or_default().push((
                    tick,
                    TickLiquidity {
                        gross: t.liquidityGross,
                        net: t.liquidityNet,
                    },
                ));
            }
        }

        partials
            .into_iter()
            .map(|p| match p {
                Partial::Ready(pool) => Ok(pool),
                Partial::Aerodrome(pool, _) => Ok(pool.into()),
                Partial::Concentrated(mut pool) => {
                    let a = pool.id().address;
                    let (_, coverage) = windows.remove(&a).expect("window recorded in round 2");
                    pool.set_ticks(ticks.remove(&a).unwrap_or_default(), coverage)?;
                    Ok(pool.into())
                }
            })
            .collect()
    }

    /// Current `fee()` of each concentrated pool in `pools`, at the latest
    /// block, in hundredths of a basis point.
    pub async fn fees(&self, pools: &[Address]) -> Result<Vec<(Address, u32)>> {
        let calls: Vec<_> = pools.iter().map(|&a| call(a, abi::feeCall {})).collect();
        let results = self.multicall(&calls, BlockId::latest()).await?;
        pools
            .iter()
            .zip(&results)
            .map(|(&a, data)| Ok((a, decode::<abi::feeCall>(a, data)?.to::<u32>())))
            .collect()
    }
}

fn next_of<'a>(it: &mut impl Iterator<Item = &'a Bytes>) -> Result<&'a Bytes> {
    it.next()
        .ok_or_else(|| KairosError::Internal("multicall result count".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{aliases::I24, hex};

    /// Slipstream's `ticks()` returns ten words and its `slot0()` six; both
    /// decode with the shorter Uniswap-compatible declarations.
    #[test]
    fn trailing_return_fields_are_ignored() {
        let mut ticks = vec![0u8; 32 * 10];
        ticks[31] = 7; // liquidityGross = 7
        ticks[32..64].fill(0xff); // liquidityNet = -1
        let t = abi::ticksCall::abi_decode_returns(&ticks).unwrap();
        assert_eq!((t.liquidityGross, t.liquidityNet), (7, -1));

        // getReserves() on Aerodrome: three uint256 words.
        let reserves = hex!(
            "00000000000000000000000000000000000000000000000b4868dea588f47b69"
            "0000000000000000000000000000000000000000000000000000004fb833a3ba"
            "000000000000000000000000000000000000000000000000000000006b06d601"
        );
        let r = abi::getReservesCall::abi_decode_returns(&reserves).unwrap();
        assert_eq!(r.reserve1, U256::from(0x4fb833a3bau64));

        let mut slot0 = vec![0u8; 32 * 6];
        slot0[31] = 1;
        slot0[32..64].fill(0xff); // tick = -1
        let s = abi::slot0Call::abi_decode_returns(&slot0).unwrap();
        assert_eq!(s.tick, I24::try_from(-1).unwrap());
    }

    #[test]
    fn tick_window_covers_whole_words() {
        // Spacing 100, tick -202272: compressed -2023, word -8.
        let (words, coverage) = tick_window(-202_272, 100, 2);
        assert_eq!(words, -10..=-6);
        assert_eq!(coverage, -256_000..=-128_100);
        assert!(coverage.contains(&-202_272));

        // Spacing 1 near zero: word -1 for negative ticks.
        let (words, coverage) = tick_window(-1, 1, 1);
        assert_eq!(words, -2..=0);
        assert_eq!(coverage, -512..=255);
    }

    #[test]
    fn tick_window_clamps_at_tick_bounds() {
        let (words, coverage) = tick_window(MIN_TICK, 200, 3);
        assert_eq!(*coverage.start(), MIN_TICK);
        assert_eq!(
            *words.start(),
            MIN_TICK.div_euclid(200).div_euclid(256) as i16
        );
        let (_, coverage) = tick_window(MAX_TICK, 1, 3);
        assert_eq!(*coverage.end(), MAX_TICK);
    }
}
