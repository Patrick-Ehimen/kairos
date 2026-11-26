//! What a transaction costs on a chain.
//!
//! A [`GasModel`] turns a route's execution gas and transaction size into a
//! fee in wei. Execution gas alone undercounts on rollups, which also charge
//! for publishing the transaction's data to L1, so each chain family has its
//! own model:
//!
//! - [`ExecutionFees`]: execution gas only.
//! - [`OpStackFees`]: OP Stack chains from the Fjord upgrade on: execution
//!   gas, the L1 data fee and the operator fee.
//!
//! Models hold fee parameters read from the chain at one point in time, so
//! pricing a route is pure arithmetic with no RPC call. The caller refreshes
//! them.

use alloy::primitives::{Address, U256};
use serde::Deserialize;

/// Prices a transaction from fee parameters read from the chain.
pub trait GasModel {
    /// Fee of a transaction that uses `gas_used` execution gas and whose
    /// unsigned EIP-2718 encoding is `unsigned_tx_size` bytes.
    fn fee(&self, gas_used: u64, unsigned_tx_size: usize) -> TxFee;
}

/// A transaction fee, in wei, by component.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxFee {
    /// Execution gas times the gas price.
    pub execution_wei: U256,
    /// Rollup L1 data fee. Zero on chains without one.
    pub l1_data_wei: U256,
    /// Rollup operator fee. Zero on chains without one.
    pub operator_wei: U256,
}

impl TxFee {
    /// Sum of all components.
    pub fn total(&self) -> U256 {
        self.execution_wei + self.l1_data_wei + self.operator_wei
    }
}

/// Where a chain's data-fee parameters are read from, as set in the chain
/// profile's `[data_fee]` table. Chains without the table pay execution gas
/// only.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DataFeeConfig {
    /// OP Stack: fee parameters from the `GasPriceOracle` and `L1Block`
    /// predeploys.
    OpStack {
        /// `GasPriceOracle` predeploy.
        gas_price_oracle: Address,
        /// `L1Block` predeploy, which holds the operator-fee parameters.
        l1_block: Address,
    },
}

/// Live fees of one chain, whichever model it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainFees {
    /// See [`ExecutionFees`].
    Execution(ExecutionFees),
    /// See [`OpStackFees`].
    OpStack(OpStackFees),
}

impl ChainFees {
    /// Gas price the execution component is priced at, in wei.
    pub fn gas_price_wei(&self) -> u128 {
        match self {
            Self::Execution(f) => f.gas_price_wei,
            Self::OpStack(f) => f.gas_price_wei,
        }
    }
}

impl GasModel for ChainFees {
    fn fee(&self, gas_used: u64, unsigned_tx_size: usize) -> TxFee {
        match self {
            Self::Execution(f) => f.fee(gas_used, unsigned_tx_size),
            Self::OpStack(f) => f.fee(gas_used, unsigned_tx_size),
        }
    }
}

/// Execution gas at a single gas price, with no other charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionFees {
    /// Price per unit of execution gas, in wei.
    pub gas_price_wei: u128,
}

impl GasModel for ExecutionFees {
    fn fee(&self, gas_used: u64, _unsigned_tx_size: usize) -> TxFee {
        TxFee {
            execution_wei: execution_wei(gas_used, self.gas_price_wei),
            ..TxFee::default()
        }
    }
}

fn execution_wei(gas_used: u64, gas_price_wei: u128) -> U256 {
    U256::from(gas_used) * U256::from(gas_price_wei)
}

/// Fee parameters of an OP Stack chain at Fjord or later, read from its
/// `GasPriceOracle` and `L1Block` predeploys.
///
/// The L1 data fee is charged on the transaction's compressed size. Kairos
/// prices routes before it has signed bytes to compress, so it uses the same
/// upper bound as the oracle's `getL1FeeUpperBound`, which assumes data that
/// barely compresses. Executor calldata is mostly zero padding and compresses
/// well, so the real fee is lower.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpStackFees {
    /// Price per unit of L2 execution gas, in wei.
    pub gas_price_wei: u128,
    /// L1 base fee, in wei (`l1BaseFee()`).
    pub l1_base_fee: U256,
    /// L1 blob base fee, in wei (`blobBaseFee()`).
    pub blob_base_fee: U256,
    /// `baseFeeScalar()`.
    pub base_fee_scalar: u32,
    /// `blobBaseFeeScalar()`.
    pub blob_base_fee_scalar: u32,
    /// Operator fee, which depends on the chain's upgrade level.
    pub operator_fee: OperatorFee,
}

/// The OP Stack operator fee, charged per transaction on top of the
/// execution and L1 data fees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorFee {
    /// Before the Isthmus upgrade: no operator fee.
    None,
    /// Isthmus: `gas_used × scalar / 1e6 + constant`.
    Isthmus {
        /// `operatorFeeScalar()`.
        scalar: u32,
        /// `operatorFeeConstant()`, in wei.
        constant: u64,
    },
    /// Jovian and later: `gas_used × scalar × 100 + constant`.
    Jovian {
        /// `operatorFeeScalar()`.
        scalar: u32,
        /// `operatorFeeConstant()`, in wei.
        constant: u64,
    },
}

/// Bytes `getL1FeeUpperBound` adds to the unsigned size for the signature.
const SIGNATURE_BYTES: u64 = 68;
/// Fixed-point scale of the size regression and of the fee scalars (1e6).
const SCALE: u64 = 1_000_000;
/// Intercept of the regression from compressed size to L1 data size, scaled
/// by [`SCALE`].
const COST_INTERCEPT: i64 = -42_585_600;
/// Slope of the same regression, scaled by [`SCALE`].
const COST_FASTLZ_COEF: u64 = 836_500;
/// Smallest L1 data size charged, in bytes.
const MIN_TRANSACTION_SIZE: u64 = 100;

impl OpStackFees {
    /// Upper bound of the L1 data fee of a transaction whose unsigned
    /// encoding is `unsigned_tx_size` bytes, in wei. Equals the oracle's
    /// `getL1FeeUpperBound(unsigned_tx_size)` for the same parameters.
    pub fn l1_data_fee_upper_bound(&self, unsigned_tx_size: usize) -> U256 {
        let tx_size = unsigned_tx_size as u64 + SIGNATURE_BYTES;
        let compressed_upper_bound = tx_size + tx_size / 255 + 16;
        let estimated = (COST_INTERCEPT + (COST_FASTLZ_COEF * compressed_upper_bound) as i64)
            .max((MIN_TRANSACTION_SIZE * SCALE) as i64) as u64;
        let fee_scaled = U256::from(self.base_fee_scalar) * U256::from(16) * self.l1_base_fee
            + U256::from(self.blob_base_fee_scalar) * self.blob_base_fee;
        U256::from(estimated) * fee_scaled / U256::from(SCALE * SCALE)
    }

    /// Operator fee of a transaction that uses `gas_used` execution gas, in
    /// wei.
    pub fn operator_fee(&self, gas_used: u64) -> U256 {
        let gas = U256::from(gas_used);
        match self.operator_fee {
            OperatorFee::None => U256::ZERO,
            OperatorFee::Isthmus { scalar, constant } => (gas.saturating_mul(U256::from(scalar))
                / U256::from(SCALE))
            .saturating_add(U256::from(constant)),
            OperatorFee::Jovian { scalar, constant } => {
                gas * U256::from(scalar) * U256::from(100) + U256::from(constant)
            }
        }
    }
}

impl GasModel for OpStackFees {
    fn fee(&self, gas_used: u64, unsigned_tx_size: usize) -> TxFee {
        TxFee {
            execution_wei: execution_wei(gas_used, self.gas_price_wei),
            l1_data_wei: self.l1_data_fee_upper_bound(unsigned_tx_size),
            operator_wei: self.operator_fee(gas_used),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parameters read from Base's `GasPriceOracle` at block 62_679_287
    /// (2026-11-26).
    fn base_sample() -> OpStackFees {
        OpStackFees {
            gas_price_wei: 6_005_784,
            l1_base_fee: U256::from(59_208_556u64),
            blob_base_fee: U256::from(3_067_131u64),
            base_fee_scalar: 2269,
            blob_base_fee_scalar: 1_055_762,
            operator_fee: OperatorFee::Jovian {
                scalar: 0,
                constant: 0,
            },
        }
    }

    /// `getL1FeeUpperBound(size)` as returned by Base's oracle at the same
    /// block as [`base_sample`].
    #[test]
    fn l1_fee_matches_oracle_upper_bound() {
        let fees = base_sample();
        for (size, oracle) in [
            (0, 538_766_777u64),
            (500, 2_411_538_414),
            (964, 4_511_699_802),
            (1220, 5_669_943_314),
        ] {
            assert_eq!(
                fees.l1_data_fee_upper_bound(size),
                U256::from(oracle),
                "{size}"
            );
        }
    }

    #[test]
    fn small_transactions_pay_the_minimum_size() {
        let fees = base_sample();
        assert_eq!(
            fees.l1_data_fee_upper_bound(0),
            fees.l1_data_fee_upper_bound(10)
        );
        assert!(fees.l1_data_fee_upper_bound(1000) > fees.l1_data_fee_upper_bound(500));
    }

    #[test]
    fn operator_fee_by_upgrade() {
        let mut fees = base_sample();
        assert_eq!(fees.operator_fee(1_000_000), U256::ZERO);
        fees.operator_fee = OperatorFee::Isthmus {
            scalar: 2_000_000,
            constant: 7,
        };
        assert_eq!(fees.operator_fee(1_000), U256::from(2_007));
        fees.operator_fee = OperatorFee::Jovian {
            scalar: 3,
            constant: 7,
        };
        assert_eq!(fees.operator_fee(1_000), U256::from(300_007));
        fees.operator_fee = OperatorFee::None;
        assert_eq!(fees.operator_fee(1_000), U256::ZERO);
    }

    #[test]
    fn total_sums_components() {
        let fees = base_sample();
        let fee = fees.fee(400_000, 900);
        assert_eq!(fee.execution_wei, U256::from(400_000u64 * 6_005_784));
        assert_eq!(fee.l1_data_wei, fees.l1_data_fee_upper_bound(900));
        assert_eq!(fee.total(), fee.execution_wei + fee.l1_data_wei);
        let flat = ExecutionFees { gas_price_wei: 10 }.fee(5, 1_000_000);
        assert_eq!(flat.total(), U256::from(50));
    }
}
