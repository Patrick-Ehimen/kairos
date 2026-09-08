// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice The closed set of AMM *math families*. Mirrors `AmmKind` in
///         `modules/core`. This enum is the only thing the executor dispatches on —
///         specific venues (Camelot, Ramses, Sushi, …) are open, per-chain
///         `protocolId` values resolved through the on-chain registry.
/// @dev    Unset == 0 is reserved as "not registered" and is always invalid.
enum AmmKind {
    Unset,
    ConstantProduct, // UniV2, Sushi, Camelot V2, Solidly volatile pairs
    ConcentratedLiquidity, // UniV3, Algebra (Camelot V3), Ramses V2, Pancake V3
    StableSwap, // Curve — pull-based, escape-hatch calldata
    WeightedPool, // Balancer V2 Vault — pull-based, escape-hatch calldata
    LiquidityBook // TraderJoe LB — bin-based, push-based
}

/// @notice A registry entry: one per-chain protocol id.
/// @param kind    Which math family the executor dispatches to.
/// @param router  Single shared entry point (Balancer Vault). address(0) for
///                per-pool venues, where `SwapStep.pool` is the target.
/// @param enabled Runtime kill switch, flipped by the off-chain risk layer.
///                Deliberately NOT part of `registryDigest` — see KairosExecutorBase.
struct Protocol {
    AmmKind kind;
    address router;
    bool enabled;
}

/// @notice One hop of a route.
/// @dev `tokenIn` and `amountIn` are absent by design: hop i's input token is
///      hop i-1's `tokenOut` (or the flash-loaned asset for hop 0), and its input
///      amount is hop i-1's *measured* output. Both are derived on-chain, which
///      removes ~64 bytes/hop of L1-posted calldata and structurally eliminates a
///      whole bug class: a caller-supplied input amount disagreeing with the
///      executor's real balance when a pull-based venue pulls an amount baked
///      into its own calldata.
/// @param protocolId   Per-chain id; must resolve in the registry and be enabled.
/// @param feeP         Pool fee in parts-per-million (UniV3 units: 3000 == 0.30%).
///                     Read only for `ConstantProduct`; MUST be 0 otherwise.
/// @param pool         Pool/pair address.
/// @param tokenOut     Token this hop produces.
/// @param minAmountOut Hard slippage floor, enforced against the measured balance
///                     delta. Under best-effort inclusion this is the primary
///                     loss-prevention mechanism, not a formality.
/// @param data         Escape hatch. Empty ⇒ the executor encodes the call itself.
///                     Non-empty ⇒ raw calldata for StableSwap / WeightedPool.
///                     Rejected for ConcentratedLiquidity (callback state).
struct SwapStep {
    uint16 protocolId;
    uint24 feeP;
    address pool;
    address tokenOut;
    uint256 minAmountOut;
    bytes data;
}
