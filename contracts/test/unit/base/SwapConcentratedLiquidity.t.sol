// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {KairosExecutorBase} from "src/KairosExecutorBase.sol";
import {SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";

import {MockV3Pool} from "test/mocks/MockV3Pool.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice Concentrated-liquidity hops and the callback that pays for them.
contract SwapConcentratedLiquidityTest is ExecutorTestBase {
    /// @dev Reaching `InsufficientOutput` proves the swap completed — including the pool's
    ///      own check that the callback paid in full — and pins the output to the wei.
    function _assertHopOutput(MockV3Pool pool, address tokenIn, address tokenOut) internal {
        uint256 expected = BORROW * pool.rateNum() / pool.rateDen();
        SwapStep memory hop = _step(UNI_V3, address(pool), tokenOut);
        hop.minAmountOut = expected + 1;
        SwapStep[] memory steps = _pair(hop, _step(UNI_V3, address(v3), tokenIn));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.InsufficientOutput.selector, 0, expected, expected + 1));
        l2.executeArb(steps, tokenIn, BORROW, 1, _deadline());
    }

    function test_SwapPaysAndReceives_SellingWeth() public {
        _assertHopOutput(v3, address(weth), address(usd));
    }

    function test_SwapPaysAndReceives_SellingUsd() public {
        _assertHopOutput(v3, address(usd), address(weth));
    }

    function test_CycleExecutesInBothTokenOrientations() public {
        MockV3Pool up = new MockV3Pool(address(weth), address(usd), 2_000, 1);
        MockV3Pool down = new MockV3Pool(address(weth), address(usd), 1, 1_900);
        _fund(address(up));
        _fund(address(down));

        uint256 wethBefore = weth.balanceOf(owner);
        _executeL2(
            _pair(_step(UNI_V3, address(up), address(usd)), _step(UNI_V3, address(down), address(weth))),
            address(weth),
            BORROW,
            1
        );
        assertGt(weth.balanceOf(owner), wethBefore, "WETH-denominated cycle");

        // Opposite zeroForOne on both pools.
        uint256 usdBefore = usd.balanceOf(owner);
        _executeL2(
            _pair(_step(UNI_V3, address(up), address(weth)), _step(UNI_V3, address(down), address(usd))),
            address(usd),
            0.1e18,
            1
        );
        assertGt(usd.balanceOf(owner), usdBefore, "USD-denominated cycle");
    }

    function test_EveryForkCallbackSelectorIsAccepted() public {
        bytes4[4] memory selectors = [
            KairosExecutorBase.uniswapV3SwapCallback.selector,
            KairosExecutorBase.algebraSwapCallback.selector,
            KairosExecutorBase.ramsesV2SwapCallback.selector,
            KairosExecutorBase.pancakeV3SwapCallback.selector
        ];
        for (uint256 i = 0; i < selectors.length; ++i) {
            v3.setCallbackSelector(selectors[i]);
            uint256 before = weth.balanceOf(owner);
            _executeL2(_route());
            assertGt(weth.balanceOf(owner), before);
        }
    }

    function test_RevertIf_PoolDemandsMoreThanTheHopInput() public {
        v3.setBehaviour(MockV3Pool.Behaviour.Overdraw);
        SwapStep[] memory steps =
            _pair(_step(UNI_V3, address(v3), address(usd)), _v2Step(address(v2), address(weth), V2_FEE));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.CallbackOverdraw.selector, BORROW + 1, BORROW));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_PoolReportsNothingOwed() public {
        v3.setBehaviour(MockV3Pool.Behaviour.OweNothing);
        SwapStep[] memory steps =
            _pair(_step(UNI_V3, address(v3), address(usd)), _v2Step(address(v2), address(weth), V2_FEE));

        vm.prank(operator);
        vm.expectRevert(Err.NothingOwed.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_CallbackArrivesWithNoSwapInFlight() public {
        vm.expectRevert(Err.UnexpectedCallback.selector);
        v3.poke(address(l2));
    }

    function test_RevertIf_AnotherPoolCallsBackMidSwap() public {
        MockV3Pool impostor = new MockV3Pool(address(weth), address(usd), 1, 1);
        v3.setHook(address(impostor), abi.encodeCall(MockV3Pool.poke, (address(l2))));
        SwapStep[] memory steps = _route();

        vm.prank(operator);
        vm.expectRevert(Err.UnexpectedCallback.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    /// @dev If the pending-swap slots were not cleared, the pool would still be the
    ///      pending pool and this would surface as `CallbackOverdraw`, not
    ///      `UnexpectedCallback`. The exact error is what proves the clear.
    function test_RevertIf_PoolCallsBackAfterItsSwapReturned() public {
        v2.setHook(address(v3), abi.encodeCall(MockV3Pool.poke, (address(l2))));
        SwapStep[] memory steps =
            _pair(_step(UNI_V3, address(v3), address(usd)), _v2Step(address(v2), address(weth), V2_FEE));

        vm.prank(operator);
        vm.expectRevert(Err.UnexpectedCallback.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }
}
