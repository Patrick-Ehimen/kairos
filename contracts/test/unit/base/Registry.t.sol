// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Vm} from "forge-std/Vm.sol";

import {KairosExecutorL2} from "src/KairosExecutorL2.sol";
import {AmmKind, Protocol} from "src/libraries/KairosTypes.sol";
import {KairosErrors as Err} from "src/libraries/KairosErrors.sol";
import {KairosEvents as Ev} from "src/libraries/KairosEvents.sol";

import {ExecutorTestBase} from "test/utils/ExecutorTestBase.sol";

/// @notice The protocol registry and its digest — the device that lets the engine refuse
///         to arm when the on-chain id → router mapping has drifted from the chain profile.
contract RegistryTest is ExecutorTestBase {
    // ── ordering and digest ──

    function test_IdsAreSortedRegardlessOfProfileOrder() public view {
        uint16[] memory ids = l2.protocolIds();
        assertEq(ids.length, 5);
        for (uint256 i = 0; i < ids.length; ++i) {
            assertEq(ids[i], i + 1);
        }
    }

    function test_DigestMatchesProfileRecomputation() public view {
        assertEq(l2.registryDigest(), _profileDigest());
    }

    function test_DigestIsIndependentOfRegistrationOrder() public {
        (uint16[] memory ids, Protocol[] memory entries) = _profile();
        uint256 n = ids.length;
        uint16[] memory reversedIds = new uint16[](n);
        Protocol[] memory reversedEntries = new Protocol[](n);
        for (uint256 i = 0; i < n; ++i) {
            reversedIds[i] = ids[n - 1 - i];
            reversedEntries[i] = entries[n - 1 - i];
        }

        KairosExecutorL2 other = new KairosExecutorL2(owner, address(aave), reversedIds, reversedEntries);
        assertEq(other.registryDigest(), l2.registryDigest());
    }

    function test_DigestChangesWhenARouterChanges() public {
        vm.prank(owner);
        l2.setProtocol(BALANCER, AmmKind.WeightedPool, makeAddr("otherVault"));
        assertTrue(l2.registryDigest() != _profileDigest());
    }

    function test_DigestChangesWhenAKindChanges() public {
        vm.prank(owner);
        l2.setProtocol(CURVE, AmmKind.WeightedPool, address(vault));
        assertTrue(l2.registryDigest() != _profileDigest());
    }

    function test_KillSwitchDoesNotMoveTheDigest() public {
        vm.prank(owner);
        l2.setProtocolEnabled(UNI_V2, false);
        assertFalse(l2.getProtocol(UNI_V2).enabled);
        assertEq(l2.registryDigest(), _profileDigest());
    }

    function test_RegistryWritesEmitTheNewDigest() public {
        vm.recordLogs();
        vm.prank(owner);
        l2.setProtocol(9, AmmKind.ConstantProduct, address(0));

        Vm.Log[] memory logs = vm.getRecordedLogs();
        Vm.Log memory last = logs[logs.length - 1];
        assertEq(last.topics[0], Ev.RegistryDigestUpdated.selector);
        assertEq(abi.decode(last.data, (bytes32)), l2.registryDigest());
    }

    // ── writes ──

    function test_NewProtocolIsInsertedInOrderAndStartsEnabled() public {
        vm.startPrank(owner);
        l2.setProtocol(9, AmmKind.ConstantProduct, address(0));
        l2.setProtocol(7, AmmKind.ConstantProduct, address(0));
        vm.stopPrank();

        uint16[] memory ids = l2.protocolIds();
        assertEq(ids.length, 7);
        assertEq(ids[5], 7);
        assertEq(ids[6], 9);
        assertTrue(l2.getProtocol(7).enabled);
    }

    function test_UpdatingAProtocolPreservesItsKillSwitch() public {
        vm.startPrank(owner);
        l2.setProtocolEnabled(UNI_V2, false);
        l2.setProtocol(UNI_V2, AmmKind.ConstantProduct, address(0));
        vm.stopPrank();

        assertFalse(l2.getProtocol(UNI_V2).enabled);
        assertEq(l2.protocolIds().length, 5, "an update does not duplicate the id");
    }

    function test_RemoveKeepsOrderAndMovesTheDigest() public {
        vm.prank(owner);
        l2.removeProtocol(UNI_V3);

        uint16[] memory ids = l2.protocolIds();
        assertEq(ids.length, 4);
        assertEq(ids[0], UNI_V2);
        assertEq(ids[1], CURVE);
        assertEq(ids[2], BALANCER);
        assertEq(ids[3], TRADERJOE_LB);
        assertTrue(l2.getProtocol(UNI_V3).kind == AmmKind.Unset);
        assertTrue(l2.registryDigest() != _profileDigest());
    }

    function test_ReRegisteringARemovedProtocolRestoresTheDigest() public {
        vm.startPrank(owner);
        l2.removeProtocol(UNI_V3);
        l2.setProtocol(UNI_V3, AmmKind.ConcentratedLiquidity, address(0));
        vm.stopPrank();

        assertEq(l2.registryDigest(), _profileDigest());
    }

    function test_SnapshotAgreesWithGetProtocol() public view {
        (uint16[] memory ids, Protocol[] memory entries) = l2.registrySnapshot();
        assertEq(ids.length, entries.length);
        for (uint256 i = 0; i < ids.length; ++i) {
            Protocol memory p = l2.getProtocol(ids[i]);
            assertTrue(p.kind == entries[i].kind);
            assertEq(p.router, entries[i].router);
            assertEq(p.enabled, entries[i].enabled);
        }
    }

    // ── rejected writes ──

    function test_RevertIf_ProtocolIdIsZero() public {
        vm.prank(owner);
        vm.expectRevert(Err.InvalidProtocolId.selector);
        l2.setProtocol(0, AmmKind.ConstantProduct, address(0));
    }

    function test_RevertIf_KindIsUnset() public {
        vm.prank(owner);
        vm.expectRevert(Err.InvalidAmmKind.selector);
        l2.setProtocol(9, AmmKind.Unset, address(0));
    }

    function test_RevertIf_WeightedPoolHasNoRouter() public {
        vm.prank(owner);
        vm.expectRevert(Err.ZeroRouter.selector);
        l2.setProtocol(9, AmmKind.WeightedPool, address(0));
    }

    function test_RevertIf_RemovingAnUnknownProtocol() public {
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(Err.UnknownProtocol.selector, 99));
        l2.removeProtocol(99);
    }

    function test_RevertIf_TogglingAnUnknownProtocol() public {
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(Err.UnknownProtocol.selector, 99));
        l2.setProtocolEnabled(99, false);
    }

    function test_RevertIf_RegistryIsFull() public {
        vm.startPrank(owner);
        for (uint16 id = 100; id < 159; ++id) {
            l2.setProtocol(id, AmmKind.ConstantProduct, address(0));
        }
        assertEq(l2.protocolIds().length, 64);

        vm.expectRevert(Err.RegistryFull.selector);
        l2.setProtocol(500, AmmKind.ConstantProduct, address(0));
        vm.stopPrank();
    }

    function test_RevertIf_ConstructorArraysMismatch() public {
        uint16[] memory ids = new uint16[](2);
        Protocol[] memory entries = new Protocol[](1);
        vm.expectRevert(Err.ArrayLengthMismatch.selector);
        new KairosExecutorL2(owner, address(aave), ids, entries);
    }

    function test_RevertIf_ConstructorAavePoolIsZero() public {
        (uint16[] memory ids, Protocol[] memory entries) = _profile();
        vm.expectRevert(Err.ZeroAddress.selector);
        new KairosExecutorL2(owner, address(0), ids, entries);
    }

    function test_RevertIf_ConstructorProfileHasAnInvalidEntry() public {
        (uint16[] memory ids, Protocol[] memory entries) = _profile();
        ids[2] = 0;
        vm.expectRevert(Err.InvalidProtocolId.selector);
        new KairosExecutorL2(owner, address(aave), ids, entries);
    }
}
