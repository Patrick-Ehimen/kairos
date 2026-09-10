// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {SwapStep} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";

import {MockCamelotV2Pair, MockUniswapV2Pair, MockV2PairBase} from "test/mocks/MockV2Pair.sol";
import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice Constant-product hops, priced on-chain against live reserves.
contract SwapConstantProductTest is ExecutorTestBase {
    /// @dev Pins hop 0's output to the wei: demand one more than `expected` and assert the
    ///      executor reports exactly `expected` as what it received.
    function _assertHopOutput(
        MockV2PairBase pair,
        address tokenIn,
        address tokenOut,
        uint24 feeP,
        uint256 expected
    ) internal {
        SwapStep memory hop = _v2Step(address(pair), tokenOut, feeP);
        hop.minAmountOut = expected + 1;
        SwapStep[] memory steps = _pair(hop, _step(UNI_V3, address(v3), tokenIn));

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.InsufficientOutput.selector, 0, expected, expected + 1));
        l2.executeArb(steps, tokenIn, BORROW, 1, _deadline());
    }

    function _newPair(uint24 feeP) internal returns (MockUniswapV2Pair pair) {
        pair = new MockUniswapV2Pair(address(weth), address(usd), feeP);
        _fund(address(pair));
        pair.sync();
    }

    // The next two cover both zeroForOne orientations, whichever token sorts lower.

    function test_OutputMatchesUniswapsOwnFormula_SellingWeth() public {
        (uint256 reserveIn, uint256 reserveOut) = _reserves(v2, address(weth));
        uint256 uniswap = BORROW * 997 * reserveOut / (reserveIn * 1000 + BORROW * 997);
        _assertHopOutput(v2, address(weth), address(usd), V2_FEE, uniswap);
    }

    function test_OutputMatchesUniswapsOwnFormula_SellingUsd() public {
        (uint256 reserveIn, uint256 reserveOut) = _reserves(v2, address(usd));
        uint256 uniswap = BORROW * 997 * reserveOut / (reserveIn * 1000 + BORROW * 997);
        _assertHopOutput(v2, address(usd), address(weth), V2_FEE, uniswap);
    }

    function test_OutputIsExactAcrossFeeTiers() public {
        uint24[5] memory fees = [uint24(0), 500, 2_500, 3_000, 10_000];
        for (uint256 i = 0; i < fees.length; ++i) {
            MockUniswapV2Pair pair = _newPair(fees[i]);
            (uint256 reserveIn, uint256 reserveOut) = _reserves(pair, address(weth));
            _assertHopOutput(
                pair,
                address(weth),
                address(usd),
                fees[i],
                _v2AmountOut(BORROW, reserveIn, reserveOut, fees[i])
            );
        }
    }

    function test_CamelotFourWordReservesDecodeCorrectly() public {
        MockCamelotV2Pair pair = new MockCamelotV2Pair(address(weth), address(usd), V2_FEE);
        _fund(address(pair));
        pair.sync();

        (uint256 reserveIn, uint256 reserveOut) = _reserves(pair, address(weth));
        _assertHopOutput(
            pair, address(weth), address(usd), V2_FEE, _v2AmountOut(BORROW, reserveIn, reserveOut, V2_FEE)
        );
    }

    function test_RevertIf_FeeIsNotBelowTheDenominator() public {
        SwapStep[] memory steps = _route();
        steps[0].feeP = 1_000_000;

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.InvalidFee.selector, 0));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }

    function test_RevertIf_PairHasNoReserves() public {
        MockUniswapV2Pair empty = new MockUniswapV2Pair(address(weth), address(usd), V2_FEE);
        SwapStep[] memory steps = _route();
        steps[0].pool = address(empty);

        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Err.EmptyReserves.selector, 0));
        l2.executeArb(steps, address(weth), BORROW, 1, _deadline());
    }
}
