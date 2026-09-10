// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ReentrancyGuardTransient} from "@openzeppelin/contracts/utils/ReentrancyGuardTransient.sol";

import {KairosExecutorL2} from "src/KairosExecutorL2.sol";
import {SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";

import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice The flash-loan callback can only be entered through our own route, and nothing
///         can re-enter a route while one is running.
contract CallbackSecurityTest is ExecutorTestBase {
    function test_RevertIf_StrangerCallsExecuteOperation() public {
        vm.prank(stranger);
        vm.expectRevert(Err.NotAavePool.selector);
        l2.executeOperation(address(weth), BORROW, 0, address(l2), "");
    }

    function test_RevertIf_SomeoneElsesFlashLoanTargetsTheExecutor() public {
        vm.prank(stranger);
        vm.expectRevert(Err.InvalidInitiator.selector);
        aave.flashLoanSimple(address(l2), address(weth), BORROW, "", 0);
    }

    function test_RevertIf_PoolCallsExecuteOperationOutsideARoute() public {
        vm.prank(address(aave));
        vm.expectRevert(Err.NotInRoute.selector);
        l2.executeOperation(address(weth), BORROW, 0, address(l2), "");
    }

    function test_RevertIf_VenueReentersExecuteArbMidSwap() public {
        SwapStep[] memory steps = _route();

        // Make the pair an operator, so the reentrancy guard — not the operator check —
        // is what has to stop it.
        vm.prank(owner);
        l2.setOperator(address(v2), true);
        v2.setHook(
            address(l2),
            abi.encodeWithSelector(
                KairosExecutorL2.executeArb.selector, steps, address(weth), BORROW, 1, _deadline()
            )
        );

        vm.prank(operator);
        vm.expectRevert(ReentrancyGuardTransient.ReentrancyGuardReentrantCall.selector);
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }
}
