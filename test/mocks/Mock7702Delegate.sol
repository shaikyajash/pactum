// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @dev Stand-in for an EIP-7702 delegate that does NOT implement EIP-1271.
///      Tests `vm.etch` its code onto an EOA to simulate a delegated account,
///      which has code but can still sign with its own key.
contract Mock7702Delegate {
    fallback() external payable {}
}
