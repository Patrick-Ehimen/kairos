// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice Every revert reason the executor can produce, in one place so the
///         off-chain revert-rate classifier has a single source to generate from.
/// @dev A namespace only: no functions, no bytecode, nothing deployed. Errors
///      reach a contract's ABI when that contract actually reverts with them, so
///      a build that cannot produce an error never advertises it.
library KairosErrors {
    // ── access and lifecycle ──
    error ZeroAddress();
    error NotOperator();
    error Paused();
    error RescueFailed();

    // ── route validation (pre-flight, before any capital moves) ──
    error DeadlineExpired();
    error EmptyRoute();
    error RouteTooLong(uint256 length);
    error RouteNotClosed(address lastTokenOut, address flashloanToken);
    error ZeroProfitFloor();

    // ── flash-loan callback authenticity ──
    error NotAavePool();
    error InvalidInitiator();
    error NotInRoute();

    // ── protocol registry ──
    error ArrayLengthMismatch();
    error RegistryFull();
    error InvalidProtocolId();
    error InvalidAmmKind();
    error UnknownProtocol(uint16 protocolId);
    error ProtocolDisabled(uint16 protocolId);
    error ZeroRouter();

    // ── swap execution ──
    error DegenerateStep(uint256 stepIndex);
    error ZeroAmountIn(uint256 stepIndex);
    error AmountTooLarge(uint256 stepIndex);
    error InvalidFee(uint256 stepIndex);
    error EmptyReserves(uint256 stepIndex);
    error EncodingRequired(uint256 stepIndex);
    error RawCalldataUnsupported(uint256 stepIndex);
    error SwapFailed(uint256 stepIndex);
    error InsufficientOutput(uint256 stepIndex, uint256 actual, uint256 required);

    // ── settlement ──
    error InsufficientProfit(uint256 actual, uint256 required);

    // ── concentrated-liquidity callback ──
    error UnexpectedCallback();
    error NothingOwed();
    error CallbackOverdraw(uint256 owed, uint256 budget);

    // ── L1 only: referenced by KairosExecutorL1, never by L2 ──
    error TipBpsAboveMax(uint256 tipBps, uint256 maxTipBps);
    error MaxTipBpsTooHigh(uint256 maxTipBps);
}
