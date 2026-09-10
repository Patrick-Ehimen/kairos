// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

// Test double, not production code:
// - getters keep the names real protocols expose (`token0`, `tokenX`, ...)
// - casts are bounded by the balances and amounts these scenarios use
// forge-lint: disable-start(screaming-snake-case-immutable, unsafe-typecast)

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @notice Uniswap V2 pair core: factory-sorted tokens, stored reserves, and the
///         fee-adjusted constant-product check. The fee is in ppm — the same units as
///         `SwapStep.feeP` — so a test can pair any fee tier with a matching pool.
/// @dev    An optional hook runs mid-swap, after output is sent and before the invariant
///         check: the window a hostile pool would use to call back into the executor.
abstract contract MockV2PairBase {
    using SafeERC20 for IERC20;

    address public immutable token0;
    address public immutable token1;
    uint256 public immutable feeP;

    uint112 internal reserve0;
    uint112 internal reserve1;

    address public hookTarget;
    bytes public hookData;

    constructor(address tokenA, address tokenB, uint256 feeP_) {
        (token0, token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        feeP = feeP_;
    }

    function sync() external {
        reserve0 = uint112(IERC20(token0).balanceOf(address(this)));
        reserve1 = uint112(IERC20(token1).balanceOf(address(this)));
    }

    /// @notice Reserves without the variant-specific `getReserves` shape.
    function reserves() external view returns (uint112, uint112) {
        return (reserve0, reserve1);
    }

    function setHook(address target, bytes calldata data) external {
        hookTarget = target;
        hookData = data;
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata) external {
        require(amount0Out > 0 || amount1Out > 0, "V2: INSUFFICIENT_OUTPUT_AMOUNT");
        require(amount0Out < reserve0 && amount1Out < reserve1, "V2: INSUFFICIENT_LIQUIDITY");

        if (amount0Out > 0) IERC20(token0).safeTransfer(to, amount0Out);
        if (amount1Out > 0) IERC20(token1).safeTransfer(to, amount1Out);

        if (hookTarget != address(0)) {
            (bool ok, bytes memory ret) = hookTarget.call(hookData);
            if (!ok) {
                assembly ("memory-safe") {
                    revert(add(ret, 0x20), mload(ret))
                }
            }
        }

        uint256 balance0 = IERC20(token0).balanceOf(address(this));
        uint256 balance1 = IERC20(token1).balanceOf(address(this));
        uint256 amount0In = balance0 > reserve0 - amount0Out ? balance0 - (reserve0 - amount0Out) : 0;
        uint256 amount1In = balance1 > reserve1 - amount1Out ? balance1 - (reserve1 - amount1Out) : 0;
        require(amount0In > 0 || amount1In > 0, "V2: INSUFFICIENT_INPUT_AMOUNT");

        uint256 adjusted0 = balance0 * 1_000_000 - amount0In * feeP;
        uint256 adjusted1 = balance1 * 1_000_000 - amount1In * feeP;
        require(adjusted0 * adjusted1 >= uint256(reserve0) * reserve1 * 1e12, "V2: K");

        reserve0 = uint112(balance0);
        reserve1 = uint112(balance1);
    }
}

/// @notice Canonical Uniswap V2 `getReserves` shape: two reserves and a timestamp.
contract MockUniswapV2Pair is MockV2PairBase {
    constructor(address tokenA, address tokenB, uint256 feeP_) MockV2PairBase(tokenA, tokenB, feeP_) {}

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, 0);
    }
}

/// @notice Camelot V2 returns four words from `getReserves` — two reserves, then two fee
///         fields. The executor decodes only the first two; this pair proves that holds.
contract MockCamelotV2Pair is MockV2PairBase {
    constructor(address tokenA, address tokenB, uint256 feeP_) MockV2PairBase(tokenA, tokenB, feeP_) {}

    function getReserves() external view returns (uint112, uint112, uint16, uint16) {
        return (reserve0, reserve1, 300, 300);
    }
}

// forge-lint: disable-end(screaming-snake-case-immutable, unsafe-typecast)
