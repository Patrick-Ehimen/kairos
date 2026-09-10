// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

// Test double, not production code:
// - getters keep the names real protocols expose (`token0`, `tokenX`, ...)
// forge-lint: disable-start(screaming-snake-case-immutable)

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @notice Pull-based venue in the Curve / Balancer-Vault style: the amount lives in the
///         call's own calldata and is taken with `transferFrom`. Records the allowance it
///         was granted, so a test can assert the executor approved exactly the hop's input.
contract MockPullRouter {
    using SafeERC20 for IERC20;

    enum Behaviour {
        Honest,
        RevertWithReason,
        RevertEmpty
    }

    error MockRouterFailure(uint256 code);

    uint256 public immutable rateNum;
    uint256 public immutable rateDen;

    Behaviour public behaviour;
    uint256 public allowanceSeen;
    uint256 public calls;

    constructor(uint256 rateNum_, uint256 rateDen_) {
        rateNum = rateNum_;
        rateDen = rateDen_;
    }

    function setBehaviour(Behaviour behaviour_) external {
        behaviour = behaviour_;
    }

    function exchange(address tokenIn, address tokenOut, uint256 amountIn)
        external
        returns (uint256 amountOut)
    {
        if (behaviour == Behaviour.RevertWithReason) revert MockRouterFailure(42);
        if (behaviour == Behaviour.RevertEmpty) {
            assembly ("memory-safe") {
                revert(0, 0)
            }
        }

        ++calls;
        allowanceSeen = IERC20(tokenIn).allowance(msg.sender, address(this));
        IERC20(tokenIn).safeTransferFrom(msg.sender, address(this), amountIn);
        amountOut = amountIn * rateNum / rateDen;
        IERC20(tokenOut).safeTransfer(msg.sender, amountOut);
    }
}

// forge-lint: disable-end(screaming-snake-case-immutable)
