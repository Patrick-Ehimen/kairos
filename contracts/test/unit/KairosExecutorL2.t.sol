// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice What L2 is defined by: no tip apparatus, and all profit retained.
contract KairosExecutorL2Test is ExecutorTestBase {
    function test_AllProfitGoesToOwner() public {
        uint256 expected = _expectedProfit(BORROW);
        uint256 before = weth.balanceOf(owner);

        _executeL2(_route());

        assertEq(weth.balanceOf(owner) - before, expected);
    }

    function test_CoinbaseIsNeverPaid() public {
        _executeL2(_route());

        assertEq(builder.balance, 0, "no native");
        assertEq(weth.balanceOf(builder), 0, "no WETH");
        assertEq(usd.balanceOf(builder), 0, "no USD");
    }

    function test_RejectsNativeEth() public {
        vm.deal(address(this), 1 ether);
        (bool ok,) = address(l2).call{value: 1 ether}("");
        assertFalse(ok, "no receive(): L2 never handles native value");
    }
}
