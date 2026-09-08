// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {AmmKind} from "./KairosTypes.sol";

/// @notice Every event the executor emits. Same namespace-only rules as
///         KairosErrors: nothing is deployed, and an event reaches a contract's
///         ABI only if that contract emits it.
library KairosEvents {
    // ── execution ──
    /// @dev `l2GasUsed` is the execution component only. On an L2 the L1
    ///      data-availability charge is not observable from inside the EVM.
    event ArbExecuted(
        address indexed asset, uint256 flashloanAmount, uint256 premium, uint256 profit, uint256 l2GasUsed
    );

    // ── registry ──
    event ProtocolSet(uint16 indexed protocolId, AmmKind kind, address router, bool enabled);
    event ProtocolRemoved(uint16 indexed protocolId);
    event ProtocolEnabledSet(uint16 indexed protocolId, bool enabled);
    event RegistryDigestUpdated(bytes32 digest);

    // ── administration ──
    event OperatorSet(address indexed operator, bool allowed);
    event PausedSet(bool paused);
    event Rescued(address indexed token, uint256 amount);

    // ── L1 only ──
    event TipPaid(address indexed coinbase, address indexed asset, uint256 amount, bool native);
    event MaxTipBpsSet(uint256 maxTipBps);
}
