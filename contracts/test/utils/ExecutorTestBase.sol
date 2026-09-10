// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {KairosExecutorL1} from "src/KairosExecutorL1.sol";
import {KairosExecutorL2} from "src/KairosExecutorL2.sol";
import {AmmKind, Protocol, SwapStep} from "src/libraries/KairosTypes.sol";

import {MockAavePool} from "test/mocks/MockAavePool.sol";
import {MockERC20} from "test/mocks/MockERC20.sol";
import {MockLBPair} from "test/mocks/MockLBPair.sol";
import {MockPullRouter} from "test/mocks/MockPullRouter.sol";
import {MockV2PairBase, MockUniswapV2Pair} from "test/mocks/MockV2Pair.sol";
import {MockV3Pool} from "test/mocks/MockV3Pool.sol";
import {MockWETH} from "test/mocks/MockWETH.sol";

/// @notice The shared world every executor unit test runs in.
///
///         Market: WETH is 2000 USD on the V2 pair and 1900 USD on the V3 pool, so
///         borrowing WETH, selling on V2 and buying back on V3 is a closed, profitable
///         cycle. Every venue and the Aave pool hold far more than any test borrows.
///
///         Base-contract behaviour is exercised through `l2`, the simpler concrete
///         subclass. Anything L1-specific lives in its own suite.
abstract contract ExecutorTestBase is Test {
    // Per-chain protocol ids, as a chain profile would assign them.
    uint16 internal constant UNI_V2 = 1;
    uint16 internal constant UNI_V3 = 2;
    uint16 internal constant CURVE = 3;
    uint16 internal constant BALANCER = 4;
    uint16 internal constant TRADERJOE_LB = 5;

    uint256 internal constant BORROW = 1e18;
    uint24 internal constant V2_FEE = 3000; // 0.30%, in ppm
    uint256 internal constant MAX_TIP_BPS = 9_500;

    MockWETH internal weth;
    MockERC20 internal usd;
    MockAavePool internal aave;
    MockUniswapV2Pair internal v2;
    MockV3Pool internal v3;
    MockLBPair internal lb;
    MockPullRouter internal vault;

    KairosExecutorL1 internal l1;
    KairosExecutorL2 internal l2;

    address internal owner = makeAddr("owner");
    address internal operator = makeAddr("operator");
    address internal stranger = makeAddr("stranger");
    address internal builder = makeAddr("builder");

    function setUp() public virtual {
        vm.warp(1_700_000_000);
        vm.coinbase(builder);

        weth = new MockWETH();
        usd = new MockERC20("USD");
        vm.deal(address(weth), 100_000 ether);

        aave = new MockAavePool();
        _fund(address(aave));

        v2 = new MockUniswapV2Pair(address(weth), address(usd), V2_FEE);
        _fund(address(v2));
        v2.sync();

        v3 = new MockV3Pool(address(weth), address(usd), 1, 1_900);
        _fund(address(v3));

        lb = new MockLBPair(address(usd), address(weth), 1, 1_900); // X = USD, Y = WETH
        _fund(address(lb));
        lb.sync();

        vault = new MockPullRouter(1, 1_900);
        _fund(address(vault));

        (uint16[] memory ids, Protocol[] memory entries) = _profile();
        l2 = new KairosExecutorL2(owner, address(aave), ids, entries);
        l1 = new KairosExecutorL1(owner, address(aave), address(weth), MAX_TIP_BPS, ids, entries);

        vm.startPrank(owner);
        l2.setOperator(operator, true);
        l1.setOperator(operator, true);
        vm.stopPrank();

        vm.label(address(weth), "WETH");
        vm.label(address(usd), "USD");
        vm.label(address(aave), "AavePool");
        vm.label(address(v2), "V2Pair");
        vm.label(address(v3), "V3Pool");
        vm.label(address(lb), "LBPair");
        vm.label(address(vault), "Vault");
        vm.label(address(l1), "ExecutorL1");
        vm.label(address(l2), "ExecutorL2");
    }

    // ─────────────────────────── chain profile ───────────────────────────

    /// @dev The profile under test, deliberately listed out of id order.
    function _profile() internal view returns (uint16[] memory ids, Protocol[] memory entries) {
        ids = new uint16[](5);
        entries = new Protocol[](5);
        (ids[0], entries[0]) =
        (TRADERJOE_LB, Protocol({kind: AmmKind.LiquidityBook, router: address(0), enabled: true}));
        (ids[1], entries[1]) =
        (UNI_V3, Protocol({kind: AmmKind.ConcentratedLiquidity, router: address(0), enabled: true}));
        (ids[2], entries[2]) =
        (BALANCER, Protocol({kind: AmmKind.WeightedPool, router: address(vault), enabled: true}));
        (ids[3], entries[3]) =
        (UNI_V2, Protocol({kind: AmmKind.ConstantProduct, router: address(0), enabled: true}));
        (ids[4], entries[4]) =
        (CURVE, Protocol({kind: AmmKind.StableSwap, router: address(0), enabled: true}));
    }

    /// @dev What the engine computes from the profile at startup, independently of the
    ///      contract: the digest over (id, kind, router) in ascending id order.
    function _profileDigest() internal view returns (bytes32 acc) {
        acc = _link(acc, UNI_V2, AmmKind.ConstantProduct, address(0));
        acc = _link(acc, UNI_V3, AmmKind.ConcentratedLiquidity, address(0));
        acc = _link(acc, CURVE, AmmKind.StableSwap, address(0));
        acc = _link(acc, BALANCER, AmmKind.WeightedPool, address(vault));
        acc = _link(acc, TRADERJOE_LB, AmmKind.LiquidityBook, address(0));
    }

    function _link(bytes32 acc, uint16 id, AmmKind kind, address router) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(acc, id, uint8(kind), router));
    }

    // ─────────────────────────── routes ───────────────────────────

    function _step(uint16 id, address pool, address tokenOut) internal pure returns (SwapStep memory) {
        return SwapStep({protocolId: id, feeP: 0, pool: pool, tokenOut: tokenOut, minAmountOut: 1, data: ""});
    }

    function _v2Step(address pair, address tokenOut, uint24 feeP) internal pure returns (SwapStep memory) {
        return
            SwapStep({
                protocolId: UNI_V2, feeP: feeP, pool: pair, tokenOut: tokenOut, minAmountOut: 1, data: ""
            });
    }

    function _pair(SwapStep memory first, SwapStep memory second)
        internal
        pure
        returns (SwapStep[] memory steps)
    {
        steps = new SwapStep[](2);
        steps[0] = first;
        steps[1] = second;
    }

    /// @dev WETH → USD on V2, USD → WETH on V3. The canonical profitable cycle.
    function _route() internal view returns (SwapStep[] memory) {
        return _pair(_v2Step(address(v2), address(usd), V2_FEE), _step(UNI_V3, address(v3), address(weth)));
    }

    function _deadline() internal view returns (uint256) {
        return block.timestamp + 60;
    }

    function _executeL2(SwapStep[] memory steps) internal {
        _executeL2(steps, address(weth), BORROW, 1);
    }

    function _executeL2(SwapStep[] memory steps, address asset, uint256 amount, uint256 minProfit) internal {
        vm.prank(operator);
        l2.executeArb(steps, asset, amount, minProfit, _deadline());
    }

    // ─────────────────────────── independent pricing ───────────────────────────

    /// @dev Uniswap V2's `getAmountOut`, generalised from 997/1000 to a ppm fee.
    function _v2AmountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut, uint256 feeP)
        internal
        pure
        returns (uint256)
    {
        uint256 inWithFee = amountIn * (1_000_000 - feeP);
        return inWithFee * reserveOut / (reserveIn * 1_000_000 + inWithFee);
    }

    function _reserves(MockV2PairBase pair, address tokenIn)
        internal
        view
        returns (uint256 reserveIn, uint256 reserveOut)
    {
        (uint112 r0, uint112 r1) = pair.reserves();
        (reserveIn, reserveOut) =
            tokenIn == pair.token0() ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
    }

    /// @dev Profit `_route()` should realise for `amount`, from the mocks' own pricing
    ///      rules rather than from anything the executor reports. Call before executing.
    function _expectedProfit(uint256 amount) internal view returns (uint256) {
        (uint256 reserveIn, uint256 reserveOut) = _reserves(v2, address(weth));
        uint256 usdOut = _v2AmountOut(amount, reserveIn, reserveOut, V2_FEE);
        uint256 wethBack = usdOut * v3.rateNum() / v3.rateDen();
        return wethBack - amount - amount * aave.premiumBps() / 10_000;
    }

    // ─────────────────────────── setup helpers ───────────────────────────

    function _fund(address venue) internal {
        weth.mint(venue, 1_000e18);
        usd.mint(venue, 2_000_000e18);
    }

    function _deployL2(address owner_) internal returns (KairosExecutorL2) {
        (uint16[] memory ids, Protocol[] memory entries) = _profile();
        return new KairosExecutorL2(owner_, address(aave), ids, entries);
    }
}
