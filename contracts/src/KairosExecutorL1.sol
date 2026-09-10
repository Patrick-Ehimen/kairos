// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

import {KairosExecutorBase} from "./KairosExecutorBase.sol";
import {Protocol, SwapStep} from "./libraries/KairosTypes.sol";
import {KairosErrors as Err} from "./libraries/KairosErrors.sol";
import {KairosEvents as Ev} from "./libraries/KairosEvents.sol";
import {IWETH} from "./interfaces/IDex.sol";

/// @title  KairosExecutorL1
/// @notice The Ethereum L1 executor: everything in the base, plus the block-builder
///         bid. Deployed only where `InclusionMode.atomicity() == BundleAtomic`.
/// @dev    The tip exists because L1 inclusion is auctioned. It is a separate
///         contract rather than a config flag so the L2 deployment carries neither
///         the calldata nor the reachable code path — see plan/04-contracts.md.
contract KairosExecutorL1 is KairosExecutorBase {
    using SafeERC20 for IERC20;

    uint256 public constant BPS = 10_000;

    /// @notice Canonical wrapped native token, from the chain profile. Needed only
    ///         to unwrap before tipping: most builder coinbases want native ETH.
    address public immutable WRAPPED_NATIVE;

    /// @notice Owner-set ceiling on the per-call tip. The risk layer picks the
    ///         actual `tipBps` per opportunity; this bounds what it can give away.
    uint256 public maxTipBps;

    constructor(
        address owner_,
        address aavePool_,
        address wrappedNative_,
        uint256 maxTipBps_,
        uint16[] memory ids,
        Protocol[] memory protocols_
    ) KairosExecutorBase(owner_, aavePool_, ids, protocols_) {
        if (wrappedNative_ == address(0)) revert Err.ZeroAddress();
        WRAPPED_NATIVE = wrappedNative_;
        _setMaxTipBps(maxTipBps_);
    }

    function setMaxTipBps(uint256 maxTipBps_) external onlyOwner {
        _setMaxTipBps(maxTipBps_);
    }

    /// @param tipBps Share of gross profit bid to `block.coinbase`, in basis points.
    function executeArb(
        SwapStep[] calldata steps,
        address flashloanToken,
        uint256 flashloanAmount,
        uint256 minProfitOut,
        uint256 deadline,
        uint256 tipBps
    ) external onlyOperator {
        if (tipBps > maxTipBps) revert Err.TipBpsAboveMax(tipBps, maxTipBps);
        _executeRoute(steps, flashloanToken, flashloanAmount, minProfitOut, deadline, abi.encode(tipBps));
    }

    function _distribute(address asset, uint256 profit, bytes memory hookData) internal override {
        uint256 tipBps = abi.decode(hookData, (uint256));
        uint256 tip = (profit * tipBps) / BPS;
        uint256 kept;
        unchecked {
            kept = profit - tip;
        }

        if (tip > 0) _payCoinbase(asset, tip);
        if (kept > 0) IERC20(asset).safeTransfer(owner(), kept);
    }

    /// @dev Prefer native ETH; some builders run contract coinbases that reject a
    ///      plain transfer, so re-wrap and send WETH on failure.
    function _payCoinbase(address asset, uint256 tip) private {
        if (asset != WRAPPED_NATIVE) {
            // Non-native tip. Builders will not prioritise this, but it beats
            // stranding the bid.
            IERC20(asset).safeTransfer(block.coinbase, tip);
            emit Ev.TipPaid(block.coinbase, asset, tip, false);
            return;
        }

        IWETH(WRAPPED_NATIVE).withdraw(tip);
        (bool sent,) = block.coinbase.call{value: tip}("");
        if (sent) {
            emit Ev.TipPaid(block.coinbase, address(0), tip, true);
            return;
        }

        IWETH(WRAPPED_NATIVE).deposit{value: tip}();
        IERC20(WRAPPED_NATIVE).safeTransfer(block.coinbase, tip);
        emit Ev.TipPaid(block.coinbase, WRAPPED_NATIVE, tip, false);
    }

    function _setMaxTipBps(uint256 maxTipBps_) private {
        if (maxTipBps_ > BPS) revert Err.MaxTipBpsTooHigh(maxTipBps_);
        maxTipBps = maxTipBps_;
        emit Ev.MaxTipBpsSet(maxTipBps_);
    }

    /// @notice Accept native ETH — required for the unwrap step of the tip path.
    receive() external payable {}
}
