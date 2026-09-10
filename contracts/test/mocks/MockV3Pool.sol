// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

// Test double, not production code:
// - getters keep the names real protocols expose (`token0`, `tokenX`, ...)
// - casts are bounded by the balances and amounts these scenarios use
// forge-lint: disable-start(screaming-snake-case-immutable, unsafe-typecast)

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @notice Concentrated-liquidity pool at a fixed rate in both directions:
///         `out = in * rateNum / rateDen` whichever token goes in. Like a real V3 pool it
///         sends output first, then demands input through a callback — here with a
///         configurable selector, so each fork's callback name can be exercised.
/// @dev    Misbehaviours: demand one wei more than the input, report nothing owed, run a
///         hook mid-swap, or `poke` the executor's callback with no swap in flight.
contract MockV3Pool {
    using SafeERC20 for IERC20;

    enum Behaviour {
        Honest,
        Overdraw,
        OweNothing
    }

    address public immutable token0;
    address public immutable token1;
    uint256 public immutable rateNum;
    uint256 public immutable rateDen;

    bytes4 public callbackSelector = bytes4(keccak256("uniswapV3SwapCallback(int256,int256,bytes)"));
    Behaviour public behaviour;
    address public hookTarget;
    bytes public hookData;

    constructor(address tokenA, address tokenB, uint256 rateNum_, uint256 rateDen_) {
        (token0, token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        rateNum = rateNum_;
        rateDen = rateDen_;
    }

    function setCallbackSelector(bytes4 selector) external {
        callbackSelector = selector;
    }

    function setBehaviour(Behaviour behaviour_) external {
        behaviour = behaviour_;
    }

    function setHook(address target, bytes calldata data) external {
        hookTarget = target;
        hookData = data;
    }

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160, bytes calldata data)
        external
        returns (int256 amount0, int256 amount1)
    {
        require(amountSpecified > 0, "V3: exact input only");
        uint256 amountIn = uint256(amountSpecified);
        (address tokenIn, address tokenOut) = zeroForOne ? (token0, token1) : (token1, token0);
        uint256 amountOut = amountIn * rateNum / rateDen;

        IERC20(tokenOut).safeTransfer(recipient, amountOut);

        if (hookTarget != address(0)) _call(hookTarget, hookData);

        int256 owed = int256(amountIn);
        if (behaviour == Behaviour.Overdraw) owed += 1;
        else if (behaviour == Behaviour.OweNothing) owed = 0;
        (amount0, amount1) = zeroForOne ? (owed, -int256(amountOut)) : (-int256(amountOut), owed);

        uint256 balanceBefore = IERC20(tokenIn).balanceOf(address(this));
        _call(msg.sender, abi.encodeWithSelector(callbackSelector, amount0, amount1, data));
        require(IERC20(tokenIn).balanceOf(address(this)) >= balanceBefore + uint256(owed), "V3: IIA");
    }

    /// @notice Fire the callback at `executor` with no swap of ours in flight.
    function poke(address executor) external {
        _call(executor, abi.encodeWithSelector(callbackSelector, int256(1), int256(0), bytes("")));
    }

    function _call(address target, bytes memory payload) private {
        (bool ok, bytes memory ret) = target.call(payload);
        if (!ok) {
            assembly ("memory-safe") {
                revert(add(ret, 0x20), mload(ret))
            }
        }
    }
}

// forge-lint: disable-end(screaming-snake-case-immutable, unsafe-typecast)
