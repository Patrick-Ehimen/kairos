// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Vm} from "forge-std/Vm.sol";

import {KairosExecutorL1} from "src/KairosExecutorL1.sol";
import {Protocol, SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";
import {KairosEvents as Ev} from "src/libraries/KairosEvents.sol";

import {MockV3Pool} from "test/mocks/MockV3Pool.sol";
import {Rejector} from "test/mocks/Rejector.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice What L1 adds over the base: the coinbase tip and its bounds.
contract KairosExecutorL1Test is ExecutorTestBase {
    function _executeL1(SwapStep[] memory steps, address asset, uint256 amount, uint256 tipBps) internal {
        vm.prank(operator);
        l1.executeArb(steps, asset, amount, 1, _deadline(), tipBps);
    }

    // ── tip split ──

    function test_NativeTipTakesExactlyItsShareOfProfit() public {
        uint256 profit = _expectedProfit(BORROW);
        uint256 tip = profit * 9_000 / 10_000;
        uint256 before = weth.balanceOf(owner);

        vm.expectEmit(address(l1));
        emit Ev.TipPaid(builder, address(0), tip, true);
        _executeL1(_route(), address(weth), BORROW, 9_000);

        assertEq(builder.balance, tip, "coinbase paid in native ETH");
        assertEq(weth.balanceOf(owner) - before, profit - tip, "owner keeps the remainder");
        assertEq(address(l1).balance, 0, "no ETH stranded");
        assertEq(weth.balanceOf(address(l1)), 0, "no WETH stranded");
    }

    function test_ZeroTipSendsAllProfitToOwner() public {
        uint256 profit = _expectedProfit(BORROW);
        uint256 before = weth.balanceOf(owner);

        vm.recordLogs();
        _executeL1(_route(), address(weth), BORROW, 0);

        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; ++i) {
            assertTrue(logs[i].topics[0] != Ev.TipPaid.selector, "no TipPaid at zero tip");
        }
        assertEq(weth.balanceOf(owner) - before, profit);
        assertEq(builder.balance, 0);
    }

    function test_FullTipSendsAllProfitToCoinbase() public {
        vm.prank(owner);
        l1.setMaxTipBps(10_000);
        uint256 profit = _expectedProfit(BORROW);
        uint256 before = weth.balanceOf(owner);

        _executeL1(_route(), address(weth), BORROW, 10_000);

        assertEq(builder.balance, profit);
        assertEq(weth.balanceOf(owner), before);
    }

    function test_CoinbaseRefusingEthIsPaidInWrappedNative() public {
        address rejector = address(new Rejector());
        vm.coinbase(rejector);
        uint256 tip = _expectedProfit(BORROW) * 9_000 / 10_000;

        vm.expectEmit(address(l1));
        emit Ev.TipPaid(rejector, address(weth), tip, false);
        _executeL1(_route(), address(weth), BORROW, 9_000);

        assertEq(rejector.balance, 0, "native refused");
        assertEq(weth.balanceOf(rejector), tip, "re-wrapped and delivered as WETH");
        assertEq(address(l1).balance, 0, "no ETH stranded");
    }

    function test_NonNativeAssetTipIsPaidInThatToken() public {
        MockV3Pool usdBack = new MockV3Pool(address(weth), address(usd), 2_100, 1);
        _fund(address(usdBack));
        SwapStep[] memory steps =
            _pair(_v2Step(address(v2), address(weth), V2_FEE), _step(UNI_V3, address(usdBack), address(usd)));

        _executeL1(steps, address(usd), 1_000e18, 9_000);

        assertGt(usd.balanceOf(builder), 0, "tip paid in USD");
        assertEq(builder.balance, 0, "no native involved");
    }

    // ── bounds ──

    function test_RevertIf_TipExceedsTheMax() public {
        SwapStep[] memory steps = _route();
        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.TipBpsAboveMax.selector, 9_600, MAX_TIP_BPS));
        l1.executeArb(steps, address(weth), BORROW, 1, _deadline(), 9_600);
    }

    function test_SetMaxTipBpsEmits() public {
        vm.prank(owner);
        vm.expectEmit(address(l1));
        emit Ev.MaxTipBpsSet(8_000);
        l1.setMaxTipBps(8_000);
        assertEq(l1.maxTipBps(), 8_000);
    }

    function test_RevertIf_MaxTipIsSetAboveOneHundredPercent() public {
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(Err.MaxTipBpsTooHigh.selector, 10_001));
        l1.setMaxTipBps(10_001);
    }

    function test_RevertIf_OperatorSetsTheMaxTip() public {
        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, operator));
        l1.setMaxTipBps(1);
    }

    function test_RevertIf_ConstructedWithMaxTipAboveOneHundredPercent() public {
        (uint16[] memory ids, Protocol[] memory entries) = _profile();
        vm.expectRevert(abi.encodeWithSelector(Err.MaxTipBpsTooHigh.selector, 10_001));
        new KairosExecutorL1(owner, address(aave), address(weth), 10_001, ids, entries);
    }

    function test_RevertIf_ConstructedWithoutWrappedNative() public {
        (uint16[] memory ids, Protocol[] memory entries) = _profile();
        vm.expectRevert(Err.ZeroAddress.selector);
        new KairosExecutorL1(owner, address(aave), address(0), MAX_TIP_BPS, ids, entries);
    }

    // ── native value ──

    function test_AcceptsNativeEth() public {
        vm.deal(address(this), 1 ether);
        (bool ok,) = address(l1).call{value: 1 ether}("");
        assertTrue(ok);
    }
}
