// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

import {KairosExecutorL2} from "src/KairosExecutorL2.sol";
import {AmmKind, SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";
import {KairosEvents as Ev} from "src/libraries/KairosEvents.sol";

import {Rejector} from "test/mocks/Rejector.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice Who may do what: operators submit routes; only the owner administers.
contract AccessTest is ExecutorTestBase {
    function test_OwnerIsSeededAsOperator() public view {
        assertTrue(l2.isOperator(owner));
    }

    function test_OperatorCanExecute() public {
        uint256 expected = _expectedProfit(BORROW);
        uint256 before = weth.balanceOf(owner);
        _executeL2(_route());
        assertEq(weth.balanceOf(owner) - before, expected);
    }

    function test_RevertIf_StrangerExecutes() public {
        SwapStep[] memory steps = _route();
        vm.prank(stranger);
        vm.expectRevert(Err.NotOperator.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_OperatorCallsAnOwnerOnlyFunction() public {
        bytes memory unauthorized =
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, operator);
        vm.startPrank(operator);

        vm.expectRevert(unauthorized);
        l2.setProtocol(9, AmmKind.ConstantProduct, address(0));
        vm.expectRevert(unauthorized);
        l2.removeProtocol(UNI_V2);
        vm.expectRevert(unauthorized);
        l2.setProtocolEnabled(UNI_V2, false);
        vm.expectRevert(unauthorized);
        l2.setPaused(true);
        vm.expectRevert(unauthorized);
        l2.setOperator(stranger, true);
        vm.expectRevert(unauthorized);
        l2.rescue(address(weth), 1);

        vm.stopPrank();
    }

    function test_RevokedOperatorLosesAccess() public {
        vm.prank(owner);
        l2.setOperator(operator, false);

        SwapStep[] memory steps = _route();
        vm.prank(operator);
        vm.expectRevert(Err.NotOperator.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_OperatorIsZeroAddress() public {
        vm.prank(owner);
        vm.expectRevert(Err.ZeroAddress.selector);
        l2.setOperator(address(0), true);
    }

    function test_SetOperatorEmitsOnlyOnChange() public {
        vm.prank(owner);
        vm.expectEmit(address(l2));
        emit Ev.OperatorSet(stranger, true);
        l2.setOperator(stranger, true);

        vm.recordLogs();
        vm.prank(owner);
        l2.setOperator(stranger, true);
        assertEq(vm.getRecordedLogs().length, 0, "no-op write emits nothing");
    }

    function test_OwnershipTransferIsTwoStep() public {
        address newOwner = makeAddr("newOwner");
        vm.prank(owner);
        l2.transferOwnership(newOwner);
        assertEq(l2.owner(), owner, "nothing changes until accepted");

        vm.prank(newOwner);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, newOwner));
        l2.setPaused(true);

        vm.prank(newOwner);
        l2.acceptOwnership();
        assertEq(l2.owner(), newOwner);

        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, owner));
        l2.setPaused(true);
    }

    /// @dev Documents current behaviour, not an endorsement: handing over ownership does
    ///      not revoke the operator role the previous owner was seeded with.
    function test_PreviousOwnerKeepsOperatorRoleUntilRevoked() public {
        address newOwner = makeAddr("newOwner");
        vm.prank(owner);
        l2.transferOwnership(newOwner);
        vm.prank(newOwner);
        l2.acceptOwnership();

        assertTrue(l2.isOperator(owner));
    }

    function test_PauseBlocksExecutionAndUnpauseRestoresIt() public {
        SwapStep[] memory steps = _route();

        vm.prank(owner);
        l2.setPaused(true);
        vm.prank(operator);
        vm.expectRevert(Err.Paused.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());

        vm.prank(owner);
        l2.setPaused(false);
        _executeL2(steps);
    }

    function test_SetPausedEmitsOnlyOnChange() public {
        vm.prank(owner);
        vm.expectEmit(address(l2));
        emit Ev.PausedSet(true);
        l2.setPaused(true);

        vm.recordLogs();
        vm.prank(owner);
        l2.setPaused(true);
        assertEq(vm.getRecordedLogs().length, 0, "no-op write emits nothing");
    }

    function test_RescueErc20GoesToOwner() public {
        usd.mint(address(l2), 5e18);

        vm.prank(owner);
        vm.expectEmit(address(l2));
        emit Ev.Rescued(address(usd), 5e18);
        l2.rescue(address(usd), 5e18);

        assertEq(usd.balanceOf(owner), 5e18);
        assertEq(usd.balanceOf(address(l2)), 0);
    }

    function test_RescueNativeGoesToOwner() public {
        vm.deal(address(l2), 1 ether);
        vm.prank(owner);
        l2.rescue(address(0), 1 ether);
        assertEq(owner.balance, 1 ether);
    }

    function test_RevertIf_OwnerRefusesRescuedNative() public {
        address rejector = address(new Rejector());
        KairosExecutorL2 executor = _deployL2(rejector);
        vm.deal(address(executor), 1 ether);

        vm.prank(rejector);
        vm.expectRevert(Err.RescueFailed.selector);
        executor.rescue(address(0), 1 ether);
    }
}
