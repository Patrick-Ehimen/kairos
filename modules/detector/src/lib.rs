//! Arbitrage detection over a [`StateSnapshot`].
//!
//! For each new snapshot, [`Detector::detect`] looks only at cycles through
//! the pools that changed:
//!
//! 1. **Enumerate** every simple cycle of up to `max_hops` through a changed
//!    pool that starts at a configured start token (the tokens Kairos can
//!    borrow).
//! 2. **Screen** each cycle on marginal rates: it can only be profitable if
//!    the product of after-fee spot rates exceeds 1.
//! 3. **Size** survivors with the pools' exact integer quotes, finding the
//!    input that maximizes gross profit.
//! 4. **Price** the route in wei: profit is converted to the wrapped native
//!    token and the transaction's fee is subtracted. The fee comes from the
//!    chain's [`GasModel`], given the profile's execution-gas estimate and
//!    the size of the exact `executeArb` transaction the route would send,
//!    so rollup L1 data fees are included.
//!
//! These are estimates; simulation remains the authority on real profit.
//!
//! No chain constants live here: tokens and gas come from the
//! [`ChainProfile`] and [`DetectorConfig`].

mod cycles;
mod rate;
mod sizing;

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::primitives::aliases::U24;
use alloy::primitives::{Address, I256, TxKind, U256};
use kairos_core::executor::execute_arb_calldata;
use kairos_core::{ArbHop, ArbOpportunity, ChainProfile, GasEstimates, GasModel, SwapStep};
use kairos_dex::{AnyPool, Pool};
use kairos_state::StateSnapshot;
use serde::Deserialize;

use crate::cycles::{Adjacency, Hop, cycles_through};
use crate::rate::{f64_to_u256, spot_rate, u256_to_f64};

/// Detection settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DetectorConfig {
    /// Longest cycle considered, in hops (2 to 4).
    pub max_hops: usize,
    /// Tokens a cycle may start and end at, in priority order. Empty means
    /// the chain's wrapped native token only.
    pub start_tokens: Vec<Address>,
    /// Smallest net profit reported, in wei.
    pub min_profit_wei: U256,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            max_hops: 3,
            start_tokens: Vec::new(),
            min_profit_wei: U256::ZERO,
        }
    }
}

/// Finds profitable cycles in pool snapshots.
#[derive(Debug, Clone)]
pub struct Detector {
    max_hops: usize,
    start_tokens: Vec<Address>,
    min_profit_wei: U256,
    wrapped_native: Address,
    chain_id: u64,
    gas: GasEstimates,
}

impl Detector {
    /// Creates a detector for the chain described by `profile`.
    ///
    /// `max_hops` is clamped to 2..=4.
    pub fn new(config: DetectorConfig, profile: &ChainProfile) -> Self {
        let start_tokens = if config.start_tokens.is_empty() {
            vec![profile.wrapped_native]
        } else {
            config.start_tokens
        };
        Self {
            max_hops: config.max_hops.clamp(2, 4),
            start_tokens,
            min_profit_wei: config.min_profit_wei,
            wrapped_native: profile.wrapped_native,
            chain_id: profile.chain_id,
            gas: profile.gas.clone(),
        }
    }

    /// Profitable cycles through any pool in `changed`, most profitable
    /// first. `fees` prices each route's transaction; opportunities whose
    /// net profit is below `min_profit_wei` are dropped.
    pub fn detect(
        &self,
        snapshot: &StateSnapshot,
        changed: &[Address],
        fees: &impl GasModel,
    ) -> Vec<ArbOpportunity> {
        let adjacency = Adjacency::new(snapshot);
        let mut found: Vec<ArbOpportunity> = cycles_through(
            &adjacency,
            snapshot,
            changed,
            &self.start_tokens,
            self.max_hops,
        )
        .into_iter()
        .filter(|cycle| spot_log_return(snapshot, cycle).is_some_and(|r| r > 0.0))
        .filter_map(|cycle| self.price(snapshot, &cycle, fees))
        .filter(|o| o.net_profit_wei >= self.min_profit_wei)
        .collect();
        found.sort_by(|a, b| b.net_profit_wei.cmp(&a.net_profit_wei));
        found
    }

    fn price(
        &self,
        snapshot: &StateSnapshot,
        cycle: &[Hop],
        fees: &impl GasModel,
    ) -> Option<ArbOpportunity> {
        let amounts = sizing::optimal(snapshot, cycle)?;
        let start = cycle[0].token_in;
        let gross = amounts[amounts.len() - 1] - amounts[0];

        let mut hops = Vec::with_capacity(cycle.len());
        let mut total_gas = self.gas.route_overhead();
        for (i, hop) in cycle.iter().enumerate() {
            let pool = snapshot.pool(&hop.pool)?;
            let gas = self.hop_gas(pool, hop, amounts[i])?;
            total_gas += gas;
            hops.push(ArbHop {
                kind: pool.kind(),
                protocol_id: pool.id().protocol_id,
                pool_address: hop.pool,
                token_in: hop.token_in,
                token_out: hop.token_out,
                amount_in: amounts[i],
                expected_out: amounts[i + 1],
                estimated_gas: gas,
            });
        }

        let profit_wei = self.to_native(snapshot, start, gross)?;
        let fee = fees.fee(total_gas, self.unsigned_tx_size(&hops, amounts[0]));
        let net_profit_wei = profit_wei.checked_sub(fee.total())?;
        let pools: Vec<String> = cycle.iter().map(|h| h.pool.to_string()).collect();
        Some(ArbOpportunity {
            id: format!("{}:{}", snapshot.block(), pools.join(">")),
            hops,
            total_profit_wei: profit_wei,
            total_gas,
            fee,
            net_profit_wei,
            block_number: snapshot.block(),
            timestamp_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as i64),
        })
    }

    /// Size of the unsigned `executeArb` transaction for `hops`, borrowing
    /// `amount_in` of the first hop's input token. Sized for the worst case:
    /// nonce, gas limit and fee fields at their widest, so the estimate is
    /// never short.
    fn unsigned_tx_size(&self, hops: &[ArbHop], amount_in: U256) -> usize {
        // Every family the detector prices is encoded by the executor, so
        // steps carry no calldata of their own.
        let steps: Vec<SwapStep> = hops
            .iter()
            .map(|h| SwapStep {
                protocol_id: h.protocol_id,
                fee_ppm: U24::MAX,
                pool: h.pool_address,
                token_out: h.token_out,
                min_amount_out: U256::MAX,
                data: Default::default(),
            })
            .collect();
        let input = execute_arb_calldata(&steps, hops[0].token_in, amount_in, U256::MAX, U256::MAX);
        TxEip1559 {
            chain_id: self.chain_id,
            nonce: u64::MAX,
            gas_limit: u64::MAX,
            max_fee_per_gas: u128::MAX,
            max_priority_fee_per_gas: u128::MAX,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            access_list: Default::default(),
            input,
        }
        .payload_len_for_signature()
    }

    /// Gas of one hop, including initialized ticks crossed for
    /// concentrated pools. `None` if the profile has no estimate for the
    /// pool's family.
    fn hop_gas(&self, pool: &AnyPool, hop: &Hop, amount_in: U256) -> Option<u64> {
        let base = self.gas.hop(pool.kind())?;
        let ticks = match pool {
            AnyPool::ConcentratedLiquidity(p) => {
                let zero_for_one = hop.token_in == pool.tokens()[0];
                let amount = I256::try_from(amount_in).ok()?;
                p.swap(zero_for_one, amount)?.initialized_ticks_crossed
            }
            _ => 0,
        };
        Some(base + u64::from(ticks) * self.gas.per_initialized_tick)
    }

    /// `amount` of `token` in wei of the native token, at the best spot rate
    /// of any live pool pairing it with the wrapped native token.
    fn to_native(&self, snapshot: &StateSnapshot, token: Address, amount: U256) -> Option<U256> {
        if token == self.wrapped_native {
            return Some(amount);
        }
        let rate = snapshot
            .registry()
            .pools_for_pair(token, self.wrapped_native)
            .filter(|p| !snapshot.is_syncing(&p.address()))
            .filter_map(|p| spot_rate(p, token, self.wrapped_native))
            .fold(None, |best: Option<f64>, r| {
                Some(best.map_or(r, |b| b.max(r)))
            })?;
        f64_to_u256(u256_to_f64(amount) * rate)
    }
}

/// `Σ ln(rate)` over the cycle's after-fee spot rates; positive means the
/// cycle returns more than it takes for small inputs.
fn spot_log_return(snapshot: &StateSnapshot, cycle: &[Hop]) -> Option<f64> {
    cycle.iter().try_fold(0.0, |acc, hop| {
        let pool = snapshot.pool(&hop.pool)?;
        Some(acc + spot_rate(pool, hop.token_in, hop.token_out)?.ln())
    })
}

#[cfg(test)]
mod tests;
