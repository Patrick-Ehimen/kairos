// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {ReentrancyGuardTransient} from "@openzeppelin/contracts/utils/ReentrancyGuardTransient.sol";
import {TransientSlot} from "@openzeppelin/contracts/utils/TransientSlot.sol";
import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";

import {AmmKind, Protocol, SwapStep} from "./libraries/KairosTypes.sol";
import {KairosErrors as Err} from "./libraries/KairosErrors.sol";
import {KairosEvents as Ev} from "./libraries/KairosEvents.sol";
import {IAaveV3Pool, IFlashLoanSimpleReceiver} from "./interfaces/IAaveV3.sol";
import {IUniswapV2Pair, IUniswapV3Pool, ILBPair} from "./interfaces/IDex.sol";

/// @title  KairosExecutorBase
/// @notice Chain-agnostic flash-loan arbitrage executor: flash loan → swap loop →
///         repay → hand profit to `_distribute`. Every address arrives through the
///         constructor; there is not one chain constant in this file.
/// @dev    Concrete chains subclass this and supply (a) an `executeArb` entry point
///         and (b) a `_distribute` policy. See KairosExecutorL1 / KairosExecutorL2.
abstract contract KairosExecutorBase is Ownable2Step, ReentrancyGuardTransient, IFlashLoanSimpleReceiver {
    using SafeERC20 for IERC20;
    using TransientSlot for *;

    // ---------------- constants ----------------

    /// @dev Uniswap V3 TickMath bounds. Algebra and the V3 forks reuse these.
    uint160 internal constant MIN_SQRT_RATIO = 4295128739;
    uint160 internal constant MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970342;

    uint256 internal constant FEE_DENOMINATOR = 1_000_000;
    uint256 internal constant MAX_ROUTE_LEN = 8;
    uint256 internal constant MAX_PROTOCOLS = 64;

    bytes32 private constant _PENDING_POOL = keccak256("kairos.executor.transient.pendingPool");
    bytes32 private constant _PENDING_TOKEN_IN = keccak256("kairos.executor.transient.pendingTokenIn");
    bytes32 private constant _PENDING_AMOUNT_IN = keccak256("kairos.executor.transient.pendingAmountIn");

    // --------------------- storage ---------------------

    /// @notice Flash-loan pool for this chain (Aave V3 IPool), from the chain profile.
    address public immutable AAVE_POOL;

    /// @notice Global circuit breaker, flipped by the off-chain risk layer.
    bool public paused;

    /// @notice Hot signing keys permitted to submit routes. Kept distinct from
    ///         `owner()` so a compromised searcher EOA cannot rotate routers or
    ///         drain the contract.
    mapping(address => bool) public isOperator;

    mapping(uint16 => Protocol) private _protocols;
    uint16[] private _protocolIds; // kept ascending, so the digest is order-free

    /// @notice keccak accumulator over (id, kind, router) for every registered
    ///         protocol, in ascending id order. The engine recomputes this from the
    ///         chain profile at startup and refuses to arm on mismatch — a silent
    ///         id→router divergence is the highest-severity failure in this layer.
    /// @dev    `enabled` is deliberately excluded: it is runtime risk state, not
    ///         profile state, and must not make a live kill-switch look like a
    ///         config mismatch.
    bytes32 public registryDigest;

    // --------------------- modifiers ---------------------

    modifier onlyOperator() {
        _onlyOperator();
        _;
    }

    modifier whenNotPaused() {
        _whenNotPaused();
        _;
    }

    // --------------------- construction ---------------------

    /// @param owner_      Cold key: owns the registry, pause, rescue and operators.
    /// @param aavePool_   Flash-loan pool for this chain, from the chain profile.
    /// @param ids         Per-chain protocol ids, from the chain profile.
    /// @param protocols_  Parallel array of registry entries.
    /// @dev `owner_` is seeded as an operator so a fresh deployment is usable.
    ///      Production should add a dedicated hot key and revoke the owner.
    constructor(address owner_, address aavePool_, uint16[] memory ids, Protocol[] memory protocols_)
        Ownable(owner_)
    {
        if (aavePool_ == address(0)) revert Err.ZeroAddress();
        if (ids.length != protocols_.length) revert Err.ArrayLengthMismatch();
        AAVE_POOL = aavePool_;

        uint256 n = ids.length;
        for (uint256 i = 0; i < n; ++i) {
            _writeProtocol(ids[i], protocols_[i].kind, protocols_[i].router, protocols_[i].enabled);
        }
        _refreshDigest();

        isOperator[owner_] = true;
        emit Ev.OperatorSet(owner_, true);
    }

    // --------------------- administration ---------------------

    function setOperator(address operator, bool allowed) external onlyOwner {
        if (operator == address(0)) revert Err.ZeroAddress();
        if (isOperator[operator] == allowed) return;
        isOperator[operator] = allowed;
        emit Ev.OperatorSet(operator, allowed);
    }

    function setPaused(bool paused_) external onlyOwner {
        if (paused == paused_) return;
        paused = paused_;
        emit Ev.PausedSet(paused_);
    }

    /// @notice Register or update one protocol id. Changes `registryDigest`, which
    ///         will trip the engine's startup assertion until the profile matches.
    function setProtocol(uint16 id, AmmKind kind, address router) external onlyOwner {
        bool enabled = _protocols[id].kind == AmmKind.Unset ? true : _protocols[id].enabled;
        _writeProtocol(id, kind, router, enabled);
        _refreshDigest();
    }

    function removeProtocol(uint16 id) external onlyOwner {
        if (_protocols[id].kind == AmmKind.Unset) revert Err.UnknownProtocol(id);
        delete _protocols[id];

        uint256 n = _protocolIds.length;
        for (uint256 i = 0; i < n; ++i) {
            if (_protocolIds[i] != id) continue;
            for (uint256 j = i; j + 1 < n; ++j) {
                _protocolIds[j] = _protocolIds[j + 1];
            }
            _protocolIds.pop();
            break;
        }

        _refreshDigest();
        emit Ev.ProtocolRemoved(id);
    }

    /// @notice Per-protocol kill switch for the risk layer. Does not move the digest.
    function setProtocolEnabled(uint16 id, bool enabled) external onlyOwner {
        Protocol storage p = _protocols[id];
        if (p.kind == AmmKind.Unset) revert Err.UnknownProtocol(id);
        if (p.enabled == enabled) return;
        p.enabled = enabled;
        emit Ev.ProtocolEnabledSet(id, enabled);
    }

    /// @notice Owner-only emergency withdrawal. token == address(0) rescues native.
    function rescue(address token, uint256 amount) external onlyOwner {
        if (token == address(0)) {
            (bool ok,) = owner().call{value: amount}("");
            if (!ok) revert Err.RescueFailed();
        } else {
            IERC20(token).safeTransfer(owner(), amount);
        }
        emit Ev.Rescued(token, amount);
    }

    // --------------------- registry views ---------------------

    function getProtocol(uint16 id) external view returns (Protocol memory) {
        return _protocols[id];
    }

    function protocolIds() external view returns (uint16[] memory) {
        return _protocolIds;
    }

    /// @notice One-call registry read for the engine's startup assertion.
    function registrySnapshot() external view returns (uint16[] memory ids, Protocol[] memory entries) {
        ids = _protocolIds;
        uint256 n = ids.length;
        entries = new Protocol[](n);
        for (uint256 i = 0; i < n; ++i) {
            entries[i] = _protocols[ids[i]];
        }
    }

    // --------------------- route execution ---------------------

    /// @dev Subclasses wrap this with their own external entry point. `hookData` is
    ///      opaque here and interpreted by `_distribute`.
    function _executeRoute(
        SwapStep[] calldata steps,
        address flashloanToken,
        uint256 flashloanAmount,
        uint256 minProfitOut,
        uint256 deadline,
        bytes memory hookData
    ) internal nonReentrant whenNotPaused {
        // Freshness is expressed in block.timestamp on purpose. Under ArbOS
        // `block.number` reports the *L1* block, so block-delta logic written for an
        // L1 chain silently means something else on Arbitrum. Nothing in this
        // contract reads block.number.
        if (block.timestamp > deadline) revert Err.DeadlineExpired();

        uint256 n = steps.length;
        if (n == 0) revert Err.EmptyRoute();
        if (n > MAX_ROUTE_LEN) revert Err.RouteTooLong(n);

        // The route must be a closed cycle back into the borrowed asset, otherwise
        // the profit accounting below measures the wrong token.
        if (steps[n - 1].tokenOut != flashloanToken) {
            revert Err.RouteNotClosed(steps[n - 1].tokenOut, flashloanToken);
        }

        // On a best-effort chain a failed attempt costs gas plus the premium, so a
        // zero floor is never a legitimate submission.
        if (minProfitOut == 0) revert Err.ZeroProfitFloor();

        // Pre-flight the registry before any capital moves: a disabled protocol
        // discovered mid-callback burns the whole tx with the premium already owed.
        for (uint256 i = 0; i < n; ++i) {
            uint16 id = steps[i].protocolId;
            Protocol storage p = _protocols[id];
            if (p.kind == AmmKind.Unset) revert Err.UnknownProtocol(id);
            if (!p.enabled) revert Err.ProtocolDisabled(id);
        }

        bytes memory params = abi.encode(steps, minProfitOut, hookData);
        IAaveV3Pool(AAVE_POOL).flashLoanSimple(address(this), flashloanToken, flashloanAmount, params, 0);
    }

    /// @notice Aave V3 flash-loan callback.
    /// @dev No `nonReentrant` here: this runs inside `_executeRoute`'s guard and the
    ///      modifier would deadlock. Guarded instead by caller, initiator, and the
    ///      guard-entered check, which together make direct invocation impossible.
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external override returns (bool) {
        // Snapshot here so `l2GasUsed` measures on-chain work, not the calldata
        // build-up in `_executeRoute`. On an L2 this is the execution component
        // only — the L1 data-availability component is not observable on-chain.
        uint256 gasStart = gasleft();

        if (msg.sender != AAVE_POOL) revert Err.NotAavePool();
        if (initiator != address(this)) revert Err.InvalidInitiator();
        if (!_reentrancyGuardEntered()) revert Err.NotInRoute();

        // Aave has already sent the principal, so this is idle inventory plus `amount`.
        // Settlement counts profit only above it.
        uint256 startBalance = IERC20(asset).balanceOf(address(this));

        (SwapStep[] memory steps, uint256 minProfitOut, bytes memory hookData) =
            abi.decode(params, (SwapStep[], uint256, bytes));

        _runRoute(steps, asset, amount);

        uint256 profit = _settle(asset, amount, premium, minProfitOut, startBalance);
        _distribute(asset, profit, hookData);

        emit Ev.ArbExecuted(asset, amount, premium, profit, gasStart - gasleft());
        return true;
    }

    /// @dev Hop 0 spends exactly the borrowed principal; every later hop spends exactly
    ///      the previous hop's measured output.
    function _runRoute(SwapStep[] memory steps, address asset, uint256 amount) private {
        address tokenIn = asset;
        uint256 amountIn = amount;
        uint256 n = steps.length;
        for (uint256 i = 0; i < n; ++i) {
            amountIn = _executeSwap(steps[i], tokenIn, amountIn, i);
            tokenIn = steps[i].tokenOut;
        }
    }

    /// @dev Ensure the pool can pull the debt, then measure and floor-check profit.
    ///      Profit is only what the route added above the balance held when the loan
    ///      arrived: idle inventory is never paid out, never tipped away, and never lets
    ///      a losing route clear the floor.
    function _settle(
        address asset,
        uint256 amount,
        uint256 premium,
        uint256 minProfitOut,
        uint256 startBalance
    ) private returns (uint256 profit) {
        uint256 totalDebt = amount + premium;
        IERC20 token = IERC20(asset);

        if (token.allowance(address(this), AAVE_POOL) < totalDebt) {
            token.forceApprove(AAVE_POOL, type(uint256).max);
        }

        // Pre-existing inventory plus the debt: the part of the balance that is not profit.
        uint256 reserved = startBalance - amount + totalDebt;
        uint256 balance = token.balanceOf(address(this));
        if (balance < reserved + minProfitOut) {
            revert Err.InsufficientProfit(balance < reserved ? 0 : balance - reserved, minProfitOut);
        }
        unchecked {
            profit = balance - reserved;
        }
    }

    /// @dev Chain-specific profit policy. L1 splits with `block.coinbase`; L2 does not.
    function _distribute(address asset, uint256 profit, bytes memory hookData) internal virtual;

    // --------------------- swap dispatch ---------------------

    /// @dev Returns the *measured* output, which becomes the next hop's input. Using
    ///      the measured delta rather than a live balance keeps the route isolated
    ///      from any idle inventory the executor happens to hold.
    function _executeSwap(SwapStep memory step, address tokenIn, uint256 amountIn, uint256 index)
        private
        returns (uint256 amountOut)
    {
        Protocol memory p = _protocols[step.protocolId];
        // Defence in depth: `_executeRoute` already pre-flighted this, but any future
        // internal caller must be guarded too.
        if (p.kind == AmmKind.Unset) revert Err.UnknownProtocol(step.protocolId);
        if (!p.enabled) revert Err.ProtocolDisabled(step.protocolId);
        if (tokenIn == step.tokenOut) revert Err.DegenerateStep(index);
        if (amountIn == 0) revert Err.ZeroAmountIn(index);

        uint256 balanceBefore = IERC20(step.tokenOut).balanceOf(address(this));

        if (step.data.length != 0) {
            _swapRaw(step, p, tokenIn, amountIn, index);
        } else if (p.kind == AmmKind.ConstantProduct) {
            _swapConstantProduct(step, tokenIn, amountIn, index);
        } else if (p.kind == AmmKind.ConcentratedLiquidity) {
            _swapConcentrated(step, tokenIn, amountIn, index);
        } else if (p.kind == AmmKind.LiquidityBook) {
            _swapLiquidityBook(step, tokenIn, amountIn);
        } else {
            // StableSwap / WeightedPool have no universal signature to encode against.
            revert Err.EncodingRequired(index);
        }

        amountOut = IERC20(step.tokenOut).balanceOf(address(this)) - balanceBefore;
        if (amountOut < step.minAmountOut) {
            revert Err.InsufficientOutput(index, amountOut, step.minAmountOut);
        }
    }

    /// @dev Push-based. The executor prices the hop against *live* reserves rather
    ///      than trusting a caller-supplied amountOut: under best-effort inclusion
    ///      the state can move between simulation and execution, and a stale exact-out
    ///      would revert on the pool's K check. Pricing here adapts; `minAmountOut`
    ///      still rejects the genuinely-bad case.
    /// @dev Assumes factory-sorted pairs (token0 < token1) — true for every V2 fork.
    function _swapConstantProduct(SwapStep memory step, address tokenIn, uint256 amountIn, uint256 index)
        private
    {
        if (step.feeP >= FEE_DENOMINATOR) revert Err.InvalidFee(index);

        (uint256 r0, uint256 r1,) = IUniswapV2Pair(step.pool).getReserves();
        bool zeroForOne = tokenIn < step.tokenOut;
        (uint256 reserveIn, uint256 reserveOut) = zeroForOne ? (r0, r1) : (r1, r0);
        if (reserveIn == 0 || reserveOut == 0) revert Err.EmptyReserves(index);

        uint256 inAfterFee = amountIn * (FEE_DENOMINATOR - step.feeP);
        uint256 out = Math.mulDiv(inAfterFee, reserveOut, reserveIn * FEE_DENOMINATOR + inAfterFee);

        IERC20(tokenIn).safeTransfer(step.pool, amountIn);
        IUniswapV2Pair(step.pool).swap(zeroForOne ? 0 : out, zeroForOne ? out : 0, address(this), "");
    }

    /// @dev Exact-input V3-shaped swap. Every parameter is derived on-chain, so a
    ///      ConcentratedLiquidity hop carries no `data` at all.
    function _swapConcentrated(SwapStep memory step, address tokenIn, uint256 amountIn, uint256 index)
        private
    {
        if (amountIn > uint256(type(int256).max)) revert Err.AmountTooLarge(index);
        bool zeroForOne = tokenIn < step.tokenOut;

        _PENDING_POOL.asAddress().tstore(step.pool);
        _PENDING_TOKEN_IN.asAddress().tstore(tokenIn);
        _PENDING_AMOUNT_IN.asUint256().tstore(amountIn);

        IUniswapV3Pool(step.pool)
            .swap(
                address(this),
                zeroForOne,
                // casting to 'int256' is safe because amountIn is bounded above by
                // type(int256).max at the top of this function
                // forge-lint: disable-next-line(unsafe-typecast)
                int256(amountIn),
                zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1,
                ""
            );

        _PENDING_POOL.asAddress().tstore(address(0));
        _PENDING_TOKEN_IN.asAddress().tstore(address(0));
        _PENDING_AMOUNT_IN.asUint256().tstore(0);
    }

    /// @dev TraderJoe Liquidity Book: push, then let the pair compute the bins.
    function _swapLiquidityBook(SwapStep memory step, address tokenIn, uint256 amountIn) private {
        bool swapForY = ILBPair(step.pool).getTokenY() == step.tokenOut;
        IERC20(tokenIn).safeTransfer(step.pool, amountIn);
        ILBPair(step.pool).swap(swapForY, address(this));
    }

    /// @dev Escape hatch for venues with no universal signature. Pull-based targets
    ///      get an exact allowance for this hop only, reset to zero afterwards.
    ///      Because `amountIn` is the measured output of the previous hop, the
    ///      allowance is always >= whatever the caller encoded into `step.data`;
    ///      an under-spend leaves dust rather than reverting.
    function _swapRaw(
        SwapStep memory step,
        Protocol memory p,
        address tokenIn,
        uint256 amountIn,
        uint256 index
    ) private {
        if (p.kind == AmmKind.ConcentratedLiquidity) {
            revert Err.RawCalldataUnsupported(index);
        }

        bool pushBased = p.kind == AmmKind.ConstantProduct || p.kind == AmmKind.LiquidityBook;
        address target = p.router == address(0) ? step.pool : p.router;
        if (target == address(0)) revert Err.ZeroRouter();

        if (pushBased) {
            IERC20(tokenIn).safeTransfer(step.pool, amountIn);
        } else {
            IERC20(tokenIn).forceApprove(target, amountIn);
        }

        (bool ok, bytes memory ret) = target.call(step.data);
        if (!ok) _bubble(ret, index);

        if (!pushBased) IERC20(tokenIn).forceApprove(target, 0);
    }

    //  concentrated-liquidity callbacks
    // One handler, four selectors. Uniswap V3, Algebra (Camelot V3), Ramses V2 and
    // Pancake V3 all call back with the same (int256,int256,bytes) shape under
    // different names. Add a one-liner per new fork the chain profile admits.

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _payConcentratedCallback(amount0Delta, amount1Delta);
    }

    function algebraSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _payConcentratedCallback(amount0Delta, amount1Delta);
    }

    function ramsesV2SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _payConcentratedCallback(amount0Delta, amount1Delta);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _payConcentratedCallback(amount0Delta, amount1Delta);
    }

    function _payConcentratedCallback(int256 amount0Delta, int256 amount1Delta) private {
        address pool = _PENDING_POOL.asAddress().tload();
        if (pool == address(0) || msg.sender != pool) revert Err.UnexpectedCallback();

        // Exactly one delta is positive for an exact-input swap: that is what we owe.
        // Each cast sits directly behind its own `> 0` guard, so neither can truncate.
        uint256 owed;
        if (amount0Delta > 0) {
            // forge-lint: disable-next-line(unsafe-typecast)
            owed = uint256(amount0Delta);
        } else if (amount1Delta > 0) {
            // forge-lint: disable-next-line(unsafe-typecast)
            owed = uint256(amount1Delta);
        }
        if (owed == 0) revert Err.NothingOwed();

        uint256 budget = _PENDING_AMOUNT_IN.asUint256().tload();
        if (owed > budget) revert Err.CallbackOverdraw(owed, budget);

        // Zero the budget before paying: one draw per swap, no re-entry top-up.
        _PENDING_AMOUNT_IN.asUint256().tstore(0);
        IERC20(_PENDING_TOKEN_IN.asAddress().tload()).safeTransfer(msg.sender, owed);
    }

    // ---------------- internals ----------------

    function _writeProtocol(uint16 id, AmmKind kind, address router, bool enabled) private {
        if (id == 0) revert Err.InvalidProtocolId();
        if (kind == AmmKind.Unset) revert Err.InvalidAmmKind();
        // Only the single-vault family has a router that must exist; every other
        // family targets `SwapStep.pool` directly.
        if (kind == AmmKind.WeightedPool && router == address(0)) revert Err.ZeroRouter();

        Protocol storage p = _protocols[id];
        if (p.kind == AmmKind.Unset) _insertId(id);

        p.kind = kind;
        p.router = router;
        p.enabled = enabled;

        emit Ev.ProtocolSet(id, kind, router, enabled);
    }

    /// @dev Keeps `_protocolIds` ascending so `registryDigest` does not depend on
    ///      the order the operator happened to register things in.
    function _insertId(uint16 id) private {
        uint256 n = _protocolIds.length;
        if (n >= MAX_PROTOCOLS) revert Err.RegistryFull();
        _protocolIds.push(id);
        for (uint256 i = n; i > 0; --i) {
            if (_protocolIds[i - 1] <= id) break;
            _protocolIds[i] = _protocolIds[i - 1];
            _protocolIds[i - 1] = id;
        }
    }

    function _refreshDigest() private {
        bytes32 acc;
        uint256 n = _protocolIds.length;
        for (uint256 i = 0; i < n; ++i) {
            uint16 id = _protocolIds[i];
            Protocol storage p = _protocols[id];
            acc = keccak256(abi.encodePacked(acc, id, uint8(p.kind), p.router));
        }
        registryDigest = acc;
        emit Ev.RegistryDigestUpdated(acc);
    }

    function _bubble(bytes memory ret, uint256 index) private pure {
        if (ret.length == 0) revert Err.SwapFailed(index);
        assembly ("memory-safe") {
            revert(add(ret, 0x20), mload(ret))
        }
    }

    // --- Modifier Helpers --- //

    function _onlyOperator() internal view {
        if (!isOperator[msg.sender]) revert Err.NotOperator();
    }

    function _whenNotPaused() internal view {
        if (paused) revert Err.Paused();
    }
}
