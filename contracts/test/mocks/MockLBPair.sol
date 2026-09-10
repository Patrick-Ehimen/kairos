// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

// Test double, not production code:
// - getters keep the names real protocols expose (`token0`, `tokenX`, ...)
// forge-lint: disable-start(screaming-snake-case-immutable)

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @notice Liquidity Book pair at a fixed rate. Unlike V2 and V3, X and Y are whatever
///         the pair was created with — not address-sorted — which is why the executor must
///         ask `getTokenY()` instead of comparing addresses.
/// @dev    Push-based like the real pair: input is inferred from balance above reserve.
///         Swapping in the wrong direction finds no input and reverts.
contract MockLBPair {
    using SafeERC20 for IERC20;

    address public immutable tokenX;
    address public immutable tokenY;
    uint256 public immutable rateNum;
    uint256 public immutable rateDen;

    uint256 private reserveX;
    uint256 private reserveY;

    constructor(address tokenX_, address tokenY_, uint256 rateNum_, uint256 rateDen_) {
        tokenX = tokenX_;
        tokenY = tokenY_;
        rateNum = rateNum_;
        rateDen = rateDen_;
    }

    function sync() external {
        reserveX = IERC20(tokenX).balanceOf(address(this));
        reserveY = IERC20(tokenY).balanceOf(address(this));
    }

    function getTokenY() external view returns (address) {
        return tokenY;
    }

    function swap(bool swapForY, address to) external returns (bytes32 amountsOut) {
        (address tokenIn, address tokenOut, uint256 reserveIn) =
            swapForY ? (tokenX, tokenY, reserveX) : (tokenY, tokenX, reserveY);
        uint256 amountIn = IERC20(tokenIn).balanceOf(address(this)) - reserveIn;
        require(amountIn > 0, "LB: InsufficientAmountIn");

        uint256 amountOut = amountIn * rateNum / rateDen;
        IERC20(tokenOut).safeTransfer(to, amountOut);

        reserveX = IERC20(tokenX).balanceOf(address(this));
        reserveY = IERC20(tokenY).balanceOf(address(this));
        amountsOut = bytes32(amountOut);
    }
}

// forge-lint: disable-end(screaming-snake-case-immutable)
