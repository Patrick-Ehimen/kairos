// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";

import {MockLBPair} from "test/mocks/MockLBPair.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice Liquidity Book hops. A wrong `swapForY` finds no input in the pair and reverts
///         with the pair's own error, so reaching `InsufficientOutput` proves the direction.
contract SwapLiquidityBookTest is ExecutorTestBase {
    function _assertHopOutput(MockLBPair pair, address tokenIn, address tokenOut) internal {
        uint256 expected = BORROW * pair.rateNum() / pair.rateDen();
        SwapStep memory hop = _step(TRADERJOE_LB, address(pair), tokenOut);
        hop.minAmountOut = expected + 1;
        SwapStep[] memory steps = _pair(hop, _step(UNI_V3, address(v3), tokenIn));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.InsufficientOutput.selector, 0, expected, expected + 1));
        l2.executeArb(steps, tokenIn, BORROW, 1, _deadline());
    }

    function test_SwapForY() public {
        _assertHopOutput(lb, address(usd), address(weth)); // X = USD in, Y = WETH out
    }

    function test_SwapForX() public {
        _assertHopOutput(lb, address(weth), address(usd));
    }

    function test_DirectionComesFromThePairNotAddressOrder() public {
        MockLBPair flipped = new MockLBPair(address(weth), address(usd), 1, 1_900); // X = WETH, Y = USD
        _fund(address(flipped));
        flipped.sync();

        _assertHopOutput(flipped, address(weth), address(usd));
        _assertHopOutput(flipped, address(usd), address(weth));
    }

    function test_RouteThroughLiquidityBookExecutes() public {
        uint256 before = weth.balanceOf(owner);
        _executeL2(
            _pair(_v2Step(address(v2), address(usd), V2_FEE), _step(TRADERJOE_LB, address(lb), address(weth)))
        );
        assertGt(weth.balanceOf(owner), before);
    }
}
