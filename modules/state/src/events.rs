//! Decoding of the pool events that change pricing state.
//!
//! Four events cover every change a quote depends on:
//!
//! | Event      | ABI          | Applied with                                 |
//! |------------|--------------|----------------------------------------------|
//! | `Sync`     | Uniswap V2   | `ConstantProductPool::set_reserves`          |
//! | `Swap`     | Uniswap V3   | `ConcentratedLiquidityPool::set_slot0`       |
//! | `Mint`     | Uniswap V3   | `ConcentratedLiquidityPool::apply_liquidity_delta` |
//! | `Burn`     | Uniswap V3   | `ConcentratedLiquidityPool::apply_liquidity_delta` |
//!
//! V2 emits `Sync` after every swap, mint and burn, so its other events are
//! not needed. V3's remaining events (`Collect`, `Flash`, …) do not move
//! price or liquidity. Subscribe with [`TOPICS`] to receive only these.

use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use kairos_core::{KairosError, Result};

mod v2 {
    alloy::sol! {
        event Sync(uint112 reserve0, uint112 reserve1);
    }
}

mod v3 {
    alloy::sol! {
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
    }
}

/// `topic0` of every event in [`PoolEvent`], for the log subscription filter.
pub const TOPICS: [B256; 4] = [
    v2::Sync::SIGNATURE_HASH,
    v3::Swap::SIGNATURE_HASH,
    v3::Mint::SIGNATURE_HASH,
    v3::Burn::SIGNATURE_HASH,
];

/// A decoded state-changing pool event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolEvent {
    /// V2 `Sync`: the reserves after the transaction.
    Sync { reserve0: U256, reserve1: U256 },
    /// V3 `Swap`: the active state after the swap.
    Swap {
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
    },
    /// V3 `Mint` (`delta > 0`) or `Burn` (`delta < 0`) of a position over
    /// `[tick_lower, tick_upper)`.
    ModifyLiquidity {
        tick_lower: i32,
        tick_upper: i32,
        delta: i128,
    },
}

/// A [`PoolEvent`] with the position of the log that carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolLog {
    /// Pool that emitted the event.
    pub pool: Address,
    /// Block the log was included in.
    pub block_number: u64,
    /// Index of the log within its block.
    pub log_index: u64,
    /// `true` if the log was dropped by a reorg and must be undone.
    pub removed: bool,
    /// The decoded event.
    pub event: PoolEvent,
}

impl PoolLog {
    /// Position of the log in the chain, for ordering and de-duplication.
    pub fn position(&self) -> (u64, u64) {
        (self.block_number, self.log_index)
    }

    /// Decodes a log received from the node.
    ///
    /// # Errors
    ///
    /// [`KairosError::InvalidLog`] if `topic0` is not in [`TOPICS`], the
    /// payload does not match the event, the log has no block number or log
    /// index (pending logs), or a liquidity amount does not fit in `int128`.
    pub fn decode(log: &Log) -> Result<Self> {
        let invalid = |reason: String| {
            KairosError::InvalidLog(format!(
                "pool {} tx {:?}: {reason}",
                log.address(),
                log.transaction_hash
            ))
        };
        let (Some(block_number), Some(log_index)) = (log.block_number, log.log_index) else {
            return Err(invalid("missing block number or log index".into()));
        };
        let data = log.data();
        let topic0 = data
            .topics()
            .first()
            .copied()
            .ok_or_else(|| invalid("no topics".into()))?;
        let decode_err = |e: alloy::sol_types::Error| invalid(e.to_string());

        let event = match topic0 {
            v2::Sync::SIGNATURE_HASH => {
                let e = v2::Sync::decode_log_data(data).map_err(decode_err)?;
                PoolEvent::Sync {
                    reserve0: U256::from(e.reserve0),
                    reserve1: U256::from(e.reserve1),
                }
            }
            v3::Swap::SIGNATURE_HASH => {
                let e = v3::Swap::decode_log_data(data).map_err(decode_err)?;
                PoolEvent::Swap {
                    sqrt_price_x96: U256::from(e.sqrtPriceX96),
                    liquidity: e.liquidity,
                    tick: e.tick.as_i32(),
                }
            }
            v3::Mint::SIGNATURE_HASH => {
                let e = v3::Mint::decode_log_data(data).map_err(decode_err)?;
                PoolEvent::ModifyLiquidity {
                    tick_lower: e.tickLower.as_i32(),
                    tick_upper: e.tickUpper.as_i32(),
                    delta: i128::try_from(e.amount)
                        .map_err(|_| invalid(format!("mint amount {} overflows", e.amount)))?,
                }
            }
            v3::Burn::SIGNATURE_HASH => {
                let e = v3::Burn::decode_log_data(data).map_err(decode_err)?;
                PoolEvent::ModifyLiquidity {
                    tick_lower: e.tickLower.as_i32(),
                    tick_upper: e.tickUpper.as_i32(),
                    delta: -i128::try_from(e.amount)
                        .map_err(|_| invalid(format!("burn amount {} overflows", e.amount)))?,
                }
            }
            other => return Err(invalid(format!("unknown topic {other}"))),
        };

        Ok(Self {
            pool: log.address(),
            block_number,
            log_index,
            removed: log.removed,
            event,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{
        I256, LogData,
        aliases::{I24, U112, U160},
    };

    const POOL: Address = Address::repeat_byte(0xaa);

    fn rpc_log(data: LogData) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address: POOL,
                data,
            },
            block_number: Some(100),
            log_index: Some(7),
            ..Default::default()
        }
    }

    fn i24(v: i32) -> I24 {
        I24::try_from(v).unwrap()
    }

    #[test]
    fn topics_are_distinct() {
        for (i, a) in TOPICS.iter().enumerate() {
            assert!(TOPICS[i + 1..].iter().all(|b| a != b));
        }
    }

    #[test]
    fn decodes_sync() {
        let data = v2::Sync {
            reserve0: U112::from(5u64),
            reserve1: U112::from(9u64),
        }
        .encode_log_data();
        let log = PoolLog::decode(&rpc_log(data)).unwrap();
        assert_eq!(log.pool, POOL);
        assert_eq!(log.position(), (100, 7));
        assert!(!log.removed);
        assert_eq!(
            log.event,
            PoolEvent::Sync {
                reserve0: U256::from(5),
                reserve1: U256::from(9)
            }
        );
    }

    #[test]
    fn decodes_swap_with_negative_tick() {
        let data = v3::Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            amount0: I256::try_from(10).unwrap(),
            amount1: I256::try_from(-9).unwrap(),
            sqrtPriceX96: U160::from(1u64 << 40),
            liquidity: 123,
            tick: i24(-887_000),
        }
        .encode_log_data();
        assert_eq!(
            PoolLog::decode(&rpc_log(data)).unwrap().event,
            PoolEvent::Swap {
                sqrt_price_x96: U256::from(1u64 << 40),
                liquidity: 123,
                tick: -887_000
            }
        );
    }

    #[test]
    fn decodes_mint_and_burn_as_signed_deltas() {
        let mint = v3::Mint {
            sender: Address::ZERO,
            owner: Address::ZERO,
            tickLower: i24(-60),
            tickUpper: i24(120),
            amount: 77,
            amount0: U256::ZERO,
            amount1: U256::ZERO,
        }
        .encode_log_data();
        let burn = v3::Burn {
            owner: Address::ZERO,
            tickLower: i24(-60),
            tickUpper: i24(120),
            amount: 77,
            amount0: U256::ZERO,
            amount1: U256::ZERO,
        }
        .encode_log_data();
        let delta = |data| match PoolLog::decode(&rpc_log(data)).unwrap().event {
            PoolEvent::ModifyLiquidity {
                tick_lower: -60,
                tick_upper: 120,
                delta,
            } => delta,
            other => panic!("{other:?}"),
        };
        assert_eq!(delta(mint), 77);
        assert_eq!(delta(burn), -77);
    }

    #[test]
    fn oversized_burn_is_invalid() {
        let burn = v3::Burn {
            owner: Address::ZERO,
            tickLower: i24(0),
            tickUpper: i24(60),
            amount: u128::MAX,
            amount0: U256::ZERO,
            amount1: U256::ZERO,
        }
        .encode_log_data();
        assert!(matches!(
            PoolLog::decode(&rpc_log(burn)),
            Err(KairosError::InvalidLog(_))
        ));
    }

    #[test]
    fn removed_flag_is_kept() {
        let mut log = rpc_log(
            v2::Sync {
                reserve0: U112::from(1u64),
                reserve1: U112::from(1u64),
            }
            .encode_log_data(),
        );
        log.removed = true;
        assert!(PoolLog::decode(&log).unwrap().removed);
    }

    #[test]
    fn rejects_unknown_malformed_and_pending_logs() {
        let unknown = LogData::new_unchecked(vec![B256::repeat_byte(1)], Default::default());
        let truncated = LogData::new_unchecked(vec![v2::Sync::SIGNATURE_HASH], vec![0; 10].into());
        let no_topics = LogData::new_unchecked(vec![], Default::default());
        for data in [unknown, truncated, no_topics] {
            assert!(matches!(
                PoolLog::decode(&rpc_log(data)),
                Err(KairosError::InvalidLog(_))
            ));
        }

        let mut pending = rpc_log(
            v2::Sync {
                reserve0: U112::from(1u64),
                reserve1: U112::from(1u64),
            }
            .encode_log_data(),
        );
        pending.block_number = None;
        assert!(PoolLog::decode(&pending).is_err());
    }
}
