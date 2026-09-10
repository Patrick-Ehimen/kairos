// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Vm} from "forge-std/Vm.sol";

import {SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";
import {KairosEvents as Ev} from "src/libraries/KairosEvents.sol";

import {MockV3Pool} from "test/mocks/MockV3Pool.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice Repayment, profit measurement, and the loss-prevention floors.
contract SettlementTest is ExecutorTestBase {
    function test_ProfitMatchesTheIndependentReference() public {
        uint256 expected = _expectedProfit(BORROW);
        uint256 before = weth.balanceOf(owner);

        _executeL2(_route());

        assertEq(weth.balanceOf(owner) - before, expected);
        assertEq(weth.balanceOf(address(l2)), 0, "nothing stranded in the executor");
    }

    function test_PremiumIsDeductedAtThePoolsRate() public {
        aave.setPremiumBps(9);
        uint256 expected = _expectedProfit(BORROW);
        uint256 before = weth.balanceOf(owner);

        _executeL2(_route());

        assertEq(weth.balanceOf(owner) - before, expected);
    }

    function test_ArbExecutedReportsTheSettlement() public {
        uint256 expected = _expectedProfit(BORROW);
        vm.recordLogs();
        _executeL2(_route());

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool found;
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].emitter != address(l2) || logs[i].topics[0] != Ev.ArbExecuted.selector) continue;
            found = true;
            assertEq(logs[i].topics[1], bytes32(uint256(uint160(address(weth)))));
            (uint256 amount, uint256 premium, uint256 profit, uint256 l2GasUsed) =
                abi.decode(logs[i].data, (uint256, uint256, uint256, uint256));
            assertEq(amount, BORROW);
            assertEq(premium, BORROW * 5 / 10_000);
            assertEq(profit, expected);
            assertGt(l2GasUsed, 0);
        }
        assertTrue(found, "ArbExecuted emitted");
    }

    function test_RevertIf_ProfitIsBelowTheFloor() public {
        uint256 expected = _expectedProfit(BORROW);
        SwapStep[] memory steps = _route();

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.InsufficientProfit.selector, expected, expected + 1));
        l2.executeArb(steps, address(weth), BORROW, expected + 1, _deadline());
    }

    function test_RevertIf_RouteLosesMoney() public {
        MockV3Pool overpriced = new MockV3Pool(address(weth), address(usd), 1, 2_100);
        _fund(address(overpriced));
        SwapStep[] memory steps = _pair(
            _v2Step(address(v2), address(usd), V2_FEE), _step(UNI_V3, address(overpriced), address(weth))
        );

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.InsufficientProfit.selector, 0, 1));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_AnyStepMissesItsMinimum() public {
        SwapStep[] memory steps = _route();
        steps[1].minAmountOut = type(uint256).max;

        vm.prank(operator);
        vm.expectPartialRevert(Err.InsufficientOutput.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_AaveAllowanceIsGrantedOnceNotPerArb() public {
        _executeL2(_route());
        assertEq(weth.allowance(address(l2), address(aave)), type(uint256).max);

        vm.expectCall(address(weth), abi.encodeWithSelector(IERC20.approve.selector), 0);
        _executeL2(_route());
    }

    // ── idle inventory ──
    // The executor should hold nothing between arbs, but dust, a mistaken transfer, or
    // a forgotten rescue can leave a balance behind. None of it belongs to the next route.

    function test_IdleInventoryIsNeitherTradedNorCountedAsProfit() public {
        weth.mint(address(l2), 5e18);
        usd.mint(address(l2), 500e18);
        uint256 expected = _expectedProfit(BORROW);
        uint256 before = weth.balanceOf(owner);

        _executeL2(_route());

        assertEq(weth.balanceOf(owner) - before, expected, "inventory is not profit");
        assertEq(weth.balanceOf(address(l2)), 5e18, "borrowed-asset inventory untouched");
        assertEq(usd.balanceOf(address(l2)), 500e18, "intermediate-token inventory untouched");
    }

    function test_RevertIf_InventoryWouldMaskALosingRoute() public {
        weth.mint(address(l2), 5e18);
        MockV3Pool overpriced = new MockV3Pool(address(weth), address(usd), 1, 2_100);
        _fund(address(overpriced));
        SwapStep[] memory steps = _pair(
            _v2Step(address(v2), address(usd), V2_FEE), _step(UNI_V3, address(overpriced), address(weth))
        );

        vm.prank(operator);
        vm.expectPartialRevert(Err.InsufficientProfit.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }
}
