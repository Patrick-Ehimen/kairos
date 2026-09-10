// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice No `receive()` and no `fallback()`, so every plain native transfer to it fails.
///         Stands in for a builder coinbase or an owner contract that refuses ETH.
contract Rejector {}
