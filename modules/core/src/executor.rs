//! ABI of the on-chain executor's entry point.
//!
//! [`abi::executeArbCall`] mirrors `KairosExecutorL2.executeArb`, so Kairos
//! can encode the exact calldata a route will send, both to size the
//! transaction for fee estimates and to simulate and submit it.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;

use crate::types::SwapStep;

/// Solidity bindings for the executor.
pub mod abi {
    alloy::sol! {
        /// Mirrors `struct SwapStep` in `KairosTypes.sol`.
        struct SwapStep {
            uint16 protocolId;
            uint24 feeP;
            address pool;
            address tokenOut;
            uint256 minAmountOut;
            bytes data;
        }

        /// `KairosExecutorL2.executeArb`.
        function executeArb(
            SwapStep[] steps,
            address flashloanToken,
            uint256 flashloanAmount,
            uint256 minProfitOut,
            uint256 deadline
        ) external;
    }
}

impl From<&SwapStep> for abi::SwapStep {
    fn from(step: &SwapStep) -> Self {
        Self {
            protocolId: step.protocol_id.0,
            feeP: step.fee_ppm,
            pool: step.pool,
            tokenOut: step.token_out,
            minAmountOut: step.min_amount_out,
            data: step.data.clone(),
        }
    }
}

/// Calldata for `executeArb`: borrow `flashloan_amount` of `flashloan_token`,
/// run `steps`, and revert unless at least `min_profit_out` is left over
/// before `deadline` (a Unix timestamp).
pub fn execute_arb_calldata(
    steps: &[SwapStep],
    flashloan_token: Address,
    flashloan_amount: U256,
    min_profit_out: U256,
    deadline: U256,
) -> Bytes {
    abi::executeArbCall {
        steps: steps.iter().map(Into::into).collect(),
        flashloanToken: flashloan_token,
        flashloanAmount: flashloan_amount,
        minProfitOut: min_profit_out,
        deadline,
    }
    .abi_encode()
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProtocolId;
    use alloy::primitives::aliases::U24;

    fn step(data: usize) -> SwapStep {
        SwapStep {
            protocol_id: ProtocolId(1),
            fee_ppm: U24::ZERO,
            pool: Address::repeat_byte(1),
            token_out: Address::repeat_byte(2),
            min_amount_out: U256::ZERO,
            data: vec![0xab; data].into(),
        }
    }

    #[test]
    fn calldata_layout() {
        let encode = |steps: &[SwapStep]| {
            execute_arb_calldata(steps, Address::ZERO, U256::ZERO, U256::ZERO, U256::ZERO)
        };
        // Selector, five head words, the array length, then per step an
        // offset word and a tuple of six head words plus the bytes length.
        assert_eq!(encode(&[step(0), step(0)]).len(), 4 + 6 * 32 + 2 * 8 * 32);
        assert_eq!(
            encode(&[step(0), step(0), step(0)]).len(),
            4 + 6 * 32 + 3 * 8 * 32
        );
        // Step data is padded to whole words.
        assert_eq!(encode(&[step(33)]).len(), 4 + 6 * 32 + 8 * 32 + 64);
        assert_eq!(
            &encode(&[step(0)])[..4],
            abi::executeArbCall::SELECTOR.as_slice()
        );
    }
}
