// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

import {IFlashLoanSimpleReceiver} from "src/interfaces/IAaveV3.sol";

/// @notice Aave V3 `flashLoanSimple`: send the principal, call back naming the caller as
///         initiator, then pull principal plus premium. The premium is set in bps.
contract MockAavePool {
    using SafeERC20 for IERC20;

    uint256 public premiumBps = 5;

    function setPremiumBps(uint256 bps) external {
        premiumBps = bps;
    }

    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata params, uint16)
        external
    {
        uint256 premium = amount * premiumBps / 10_000;
        IERC20(asset).safeTransfer(receiver, amount);
        require(
            IFlashLoanSimpleReceiver(receiver).executeOperation(asset, amount, premium, msg.sender, params),
            "MockAavePool: callback returned false"
        );
        IERC20(asset).safeTransferFrom(receiver, address(this), amount + premium);
    }
}
