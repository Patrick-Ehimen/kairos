// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IUniswapV2Pair} from "src/interfaces/IDex.sol";
import {SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";

import {MockPullRouter} from "test/mocks/MockPullRouter.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice The raw-calldata escape hatch for venues with no universal signature.
contract SwapRawCalldataTest is ExecutorTestBase {
    MockPullRouter internal curvePool;

    function setUp() public override {
        super.setUp();
        curvePool = new MockPullRouter(1, 1_900);
        _fund(address(curvePool));
    }

    /// @dev What `_route()`'s first hop produces from BORROW — the next hop's input.
    function _firstHopOutput() internal view returns (uint256) {
        (uint256 reserveIn, uint256 reserveOut) = _reserves(v2, address(weth));
        return _v2AmountOut(BORROW, reserveIn, reserveOut, V2_FEE);
    }

    function _pullHop(uint16 id, address pool, uint256 encodedAmount)
        internal
        view
        returns (SwapStep memory hop)
    {
        hop = _step(id, pool, address(weth));
        hop.data = abi.encodeCall(MockPullRouter.exchange, (address(usd), address(weth), encodedAmount));
    }

    function _sellWethOnV2() internal view returns (SwapStep memory) {
        return _v2Step(address(v2), address(usd), V2_FEE);
    }

    // ── pull-based ──

    function test_PullVenueGetsExactlyTheHopInputThenNothing() public {
        uint256 hopInput = _firstHopOutput();
        _executeL2(_pair(_sellWethOnV2(), _pullHop(CURVE, address(curvePool), hopInput)));

        assertEq(curvePool.allowanceSeen(), hopInput, "allowance == measured hop input");
        assertEq(usd.allowance(address(l2), address(curvePool)), 0, "allowance revoked afterwards");
    }

    function test_RegisteredRouterIsTheCallTarget() public {
        uint256 hopInput = _firstHopOutput();
        address notTheVault = makeAddr("notTheVault");

        // BALANCER's registry entry points at `vault`, so the step's pool field is ignored.
        _executeL2(_pair(_sellWethOnV2(), _pullHop(BALANCER, notTheVault, hopInput)));

        assertEq(vault.calls(), 1);
        assertEq(vault.allowanceSeen(), hopInput);
        assertEq(usd.allowance(address(l2), address(vault)), 0);
    }

    function test_UnderSpendLeavesDustRatherThanReverting() public {
        uint256 hopInput = _firstHopOutput();
        uint256 dust = 1_000;
        _executeL2(_pair(_sellWethOnV2(), _pullHop(CURVE, address(curvePool), hopInput - dust)));

        assertEq(usd.balanceOf(address(l2)), dust);
        assertEq(usd.allowance(address(l2), address(curvePool)), 0);
    }

    function test_VenueRevertReasonIsBubbledUnchanged() public {
        curvePool.setBehaviour(MockPullRouter.Behaviour.RevertWithReason);
        SwapStep[] memory steps = _pair(_sellWethOnV2(), _pullHop(CURVE, address(curvePool), 1));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(MockPullRouter.MockRouterFailure.selector, 42));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_EmptyVenueRevertBecomesSwapFailed() public {
        curvePool.setBehaviour(MockPullRouter.Behaviour.RevertEmpty);
        SwapStep[] memory steps = _pair(_sellWethOnV2(), _pullHop(CURVE, address(curvePool), 1));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.SwapFailed.selector, 1));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    // ── push-based ──

    function test_PushVenueAcceptsRawCalldata() public {
        uint256 expected = _expectedProfit(BORROW);
        uint256 usdOut = _firstHopOutput();
        (uint256 amount0Out, uint256 amount1Out) =
            v2.token0() == address(weth) ? (uint256(0), usdOut) : (usdOut, uint256(0));

        SwapStep memory raw = _v2Step(address(v2), address(usd), 0);
        raw.data = abi.encodeWithSelector(
            IUniswapV2Pair.swap.selector, amount0Out, amount1Out, address(l2), bytes("")
        );

        uint256 before = weth.balanceOf(owner);
        _executeL2(_pair(raw, _step(UNI_V3, address(v3), address(weth))));
        assertEq(weth.balanceOf(owner) - before, expected);
    }

    // ── rejected ──

    function test_RevertIf_RawCalldataTargetsConcentratedLiquidity() public {
        SwapStep memory raw = _step(UNI_V3, address(v3), address(usd));
        raw.data = hex"01";
        SwapStep[] memory steps = _pair(raw, _v2Step(address(v2), address(weth), V2_FEE));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.RawCalldataUnsupported.selector, 0));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_StableSwapHasNoCalldata() public {
        SwapStep[] memory steps = _pair(_sellWethOnV2(), _step(CURVE, address(curvePool), address(weth)));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.EncodingRequired.selector, 1));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_WeightedPoolHasNoCalldata() public {
        SwapStep[] memory steps = _pair(_sellWethOnV2(), _step(BALANCER, address(vault), address(weth)));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.EncodingRequired.selector, 1));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_RawCallHasNoTarget() public {
        SwapStep[] memory steps = _pair(_sellWethOnV2(), _pullHop(CURVE, address(0), 1));

        vm.prank(operator);
        vm.expectRevert(Err.ZeroRouter.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }
}
