// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";

import {MockAavePool} from "test/mocks/MockAavePool.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice Checks that run before any capital moves.
contract RouteValidationTest is ExecutorTestBase {
    function _expectRejected(
        SwapStep[] memory steps,
        uint256 minProfit,
        uint256 deadline,
        bytes memory reason
    ) internal {
        vm.prank(operator);
        vm.expectRevert(reason);
        l2.executeArb(steps, address(weth), BORROW, minProfit, deadline);
    }

    function test_RevertIf_RouteIsEmpty() public {
        _expectRejected(new SwapStep[](0), 1, _deadline(), abi.encodeWithSelector(Err.EmptyRoute.selector));
    }

    function test_RevertIf_RouteExceedsMaxLength() public {
        SwapStep[] memory steps = new SwapStep[](9);
        for (uint256 i = 0; i < steps.length; ++i) {
            steps[i] = _step(UNI_V3, address(v3), address(weth));
        }
        _expectRejected(steps, 1, _deadline(), abi.encodeWithSelector(Err.RouteTooLong.selector, 9));
    }

    function test_MaxLengthRouteExecutes() public {
        SwapStep[] memory steps = new SwapStep[](8);
        for (uint256 i = 0; i < steps.length; i += 2) {
            steps[i] = _v2Step(address(v2), address(usd), V2_FEE);
            steps[i + 1] = _step(UNI_V3, address(v3), address(weth));
        }

        uint256 before = weth.balanceOf(owner);
        _executeL2(steps);
        assertGt(weth.balanceOf(owner), before);
    }

    function test_RevertIf_RouteDoesNotReturnToTheBorrowedAsset() public {
        SwapStep[] memory steps = _route();
        steps[1].tokenOut = address(usd);
        _expectRejected(
            steps,
            1,
            _deadline(),
            abi.encodeWithSelector(Err.RouteNotClosed.selector, address(usd), address(weth))
        );
    }

    function test_RevertIf_ProfitFloorIsZero() public {
        _expectRejected(_route(), 0, _deadline(), abi.encodeWithSelector(Err.ZeroProfitFloor.selector));
    }

    function test_RevertIf_DeadlineHasPassed() public {
        _expectRejected(
            _route(), 1, block.timestamp - 1, abi.encodeWithSelector(Err.DeadlineExpired.selector)
        );
    }

    function test_DeadlineEqualToNowIsAccepted() public {
        SwapStep[] memory steps = _route();
        vm.prank(operator);
        l2.executeArb(steps, address(weth), BORROW, 1, block.timestamp);
    }

    function test_RevertIf_ProtocolIsUnknown() public {
        SwapStep[] memory steps = _route();
        steps[1].protocolId = 99;
        _expectRejected(steps, 1, _deadline(), abi.encodeWithSelector(Err.UnknownProtocol.selector, 99));
    }

    function test_RevertIf_ProtocolIsDisabled() public {
        vm.prank(owner);
        l2.setProtocolEnabled(UNI_V3, false);
        _expectRejected(
            _route(), 1, _deadline(), abi.encodeWithSelector(Err.ProtocolDisabled.selector, UNI_V3)
        );
    }

    function test_DisabledProtocolIsCaughtBeforeBorrowing() public {
        vm.prank(owner);
        l2.setProtocolEnabled(UNI_V3, false);

        vm.expectCall(address(aave), abi.encodeWithSelector(MockAavePool.flashLoanSimple.selector), 0);
        _expectRejected(
            _route(), 1, _deadline(), abi.encodeWithSelector(Err.ProtocolDisabled.selector, UNI_V3)
        );
    }

    function test_RevertIf_StepSwapsATokenForItself() public {
        SwapStep[] memory steps = _route();
        steps[0].tokenOut = address(weth);
        _expectRejected(steps, 1, _deadline(), abi.encodeWithSelector(Err.DegenerateStep.selector, 0));
    }
}
