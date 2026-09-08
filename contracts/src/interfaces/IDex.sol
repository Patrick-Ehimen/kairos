// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @dev Camelot V2 pairs return (uint112,uint112,uint16,uint16) from getReserves().
///      Decoding the first two words is correct for both; trailing return data is
///      ignored by the ABI decoder.
interface IUniswapV2Pair {
    function getReserves()
        external
        view
        returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

/// @dev Algebra (Camelot V3), Ramses V2 and Pancake V3 share this ABI shape and
///      differ only in the callback selector — see the callbacks in KairosExecutorBase.
interface IUniswapV3Pool {
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
}

/// @dev VERIFY against the deployed LBPair version before Arbitrum deploy:
///      `getTokenY()` is LB v2.1; v2.0 pairs expose `tokenY()`.
interface ILBPair {
    function getTokenY() external view returns (address);
    function swap(bool swapForY, address to) external returns (bytes32 amountsOut);
}

interface IWETH {
    function deposit() external payable;
    /// @param wad The amount of WETH (in wei) to burn and redeem
    function withdraw(uint256 wad) external;
}
