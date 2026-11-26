//! Reads a chain's live fee parameters.
//!
//! One refresh costs an `eth_gasPrice` plus, on OP Stack chains, a single
//! multicall `eth_call` that reads every data-fee parameter at one block.
//! Routes are then priced locally from the result.

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::Provider;
use kairos_core::{
    ChainFees, ChainProfile, DataFeeConfig, ExecutionFees, KairosError, OpStackFees, OperatorFee,
    Result,
};

use crate::loader::{call, decode, multicall, optional_call, rpc_err};

mod abi {
    alloy::sol! {
        function isFjord() external view returns (bool);
        function isIsthmus() external view returns (bool);
        function isJovian() external view returns (bool);
        function l1BaseFee() external view returns (uint256);
        function blobBaseFee() external view returns (uint256);
        function baseFeeScalar() external view returns (uint32);
        function blobBaseFeeScalar() external view returns (uint32);
        function operatorFeeScalar() external view returns (uint32);
        function operatorFeeConstant() external view returns (uint64);
    }
}

/// Reads [`ChainFees`] for one chain.
#[derive(Debug, Clone)]
pub struct FeeReader<P> {
    provider: P,
    multicall: Address,
    data_fee: Option<DataFeeConfig>,
}

impl<P: Provider + Clone> FeeReader<P> {
    /// A reader for the chain described by `profile`.
    pub fn new(provider: P, profile: &ChainProfile) -> Self {
        Self {
            provider,
            multicall: profile.multicall3,
            data_fee: profile.data_fee.clone(),
        }
    }

    /// The chain's current fee parameters.
    ///
    /// # Errors
    ///
    /// [`KairosError::NodeConnectionFailed`] if an RPC call fails or returns
    /// data that does not decode; [`KairosError::ConfigError`] if an OP Stack
    /// chain is older than Fjord, whose fee formula Kairos does not model.
    pub async fn read(&self) -> Result<ChainFees> {
        let gas_price_wei = self
            .provider
            .get_gas_price()
            .await
            .map_err(|e| rpc_err("eth_gasPrice", e))?;
        match &self.data_fee {
            None => Ok(ChainFees::Execution(ExecutionFees { gas_price_wei })),
            Some(DataFeeConfig::OpStack {
                gas_price_oracle,
                l1_block,
            }) => self
                .read_op_stack(gas_price_wei, *gas_price_oracle, *l1_block)
                .await
                .map(ChainFees::OpStack),
        }
    }

    async fn read_op_stack(
        &self,
        gas_price_wei: u128,
        oracle: Address,
        l1_block: Address,
    ) -> Result<OpStackFees> {
        // The operator-fee getters exist only from Isthmus on, so they may
        // revert; `isIsthmus` says whether to use them.
        let calls = [
            call(oracle, abi::isFjordCall {}),
            call(oracle, abi::isIsthmusCall {}),
            call(oracle, abi::isJovianCall {}),
            call(oracle, abi::l1BaseFeeCall {}),
            call(oracle, abi::blobBaseFeeCall {}),
            call(oracle, abi::baseFeeScalarCall {}),
            call(oracle, abi::blobBaseFeeScalarCall {}),
            optional_call(l1_block, abi::operatorFeeScalarCall {}),
            optional_call(l1_block, abi::operatorFeeConstantCall {}),
        ];
        let r = multicall(&self.provider, self.multicall, &calls, BlockId::latest()).await?;
        let required = |i: usize| {
            r[i].as_ref()
                .ok_or_else(|| KairosError::Internal("required call failed".into()))
        };

        if !decode::<abi::isFjordCall>(oracle, required(0)?)? {
            return Err(KairosError::ConfigError(format!(
                "GasPriceOracle {oracle}: chain is older than Fjord, whose fees Kairos does not model"
            )));
        }
        let isthmus = decode::<abi::isIsthmusCall>(oracle, required(1)?)?;
        let jovian = decode::<abi::isJovianCall>(oracle, required(2)?)?;
        let operator_fee = if isthmus {
            let missing = || {
                KairosError::NodeConnectionFailed(format!(
                    "L1Block {l1_block}: operator fee unreadable on an Isthmus chain"
                ))
            };
            let scalar =
                decode::<abi::operatorFeeScalarCall>(l1_block, r[7].as_ref().ok_or_else(missing)?)?;
            let constant = decode::<abi::operatorFeeConstantCall>(
                l1_block,
                r[8].as_ref().ok_or_else(missing)?,
            )?;
            if jovian {
                OperatorFee::Jovian { scalar, constant }
            } else {
                OperatorFee::Isthmus { scalar, constant }
            }
        } else {
            OperatorFee::None
        };

        Ok(OpStackFees {
            gas_price_wei,
            l1_base_fee: decode::<abi::l1BaseFeeCall>(oracle, required(3)?)?,
            blob_base_fee: decode::<abi::blobBaseFeeCall>(oracle, required(4)?)?,
            base_fee_scalar: decode::<abi::baseFeeScalarCall>(oracle, required(5)?)?,
            blob_base_fee_scalar: decode::<abi::blobBaseFeeScalarCall>(oracle, required(6)?)?,
            operator_fee,
        })
    }
}
