// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

import {KairosExecutorBase} from "./KairosExecutorBase.sol";
import {Protocol, SwapStep} from "./libraries/KairosTypes.sol";

/// @title  KairosExecutorL2
/// @notice Executor for best-effort chains — Arbitrum One, OP-stack, and anything
///         else whose `InclusionMode.atomicity()` is `BestEffort`.
/// @dev    There is no builder auction to bid into, so the whole tip apparatus is
///         absent: no `block.coinbase`, no `tipBps` calldata word, no `receive()`,
///         no wrapped-native immutable, and no tip error or event in this
///         contract's ABI. All gross profit is retained. This is the core economic
///         argument for the L2 target and the reason the contracts are split
///         rather than config-gated.
contract KairosExecutorL2 is KairosExecutorBase {
    using SafeERC20 for IERC20;

    constructor(address owner_, address aavePool_, uint16[] memory ids, Protocol[] memory protocols_)
        KairosExecutorBase(owner_, aavePool_, ids, protocols_)
    {}

    function executeArb(
        SwapStep[] calldata steps,
        address flashloanToken,
        uint256 flashloanAmount,
        uint256 minProfitOut,
        uint256 deadline
    ) external onlyOperator {
        _executeRoute(steps, flashloanToken, flashloanAmount, minProfitOut, deadline, "");
    }

    function _distribute(address asset, uint256 profit, bytes memory) internal override {
        IERC20(asset).safeTransfer(owner(), profit);
    }

    // Deliberately no `receive()`. This contract never handles native value, which
    // is the property the "no reachable path to block.coinbase" test asserts.
}
