// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @dev EIP-2612 subset used to turn a maker's approval into a signature.
interface IERC20Permit {
    function permit(
        address owner,
        address spender,
        uint256 value,
        uint256 deadline,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external;

    function allowance(address owner, address spender) external view returns (uint256);
}
