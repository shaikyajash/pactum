// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @dev Minimal EIP-1271 smart wallet: valid iff `owner` signed the digest.
contract MockWallet {
    address public immutable owner;

    constructor(address _owner) {
        owner = _owner;
    }

    function approve(address token, address spender) external {
        (bool ok,) = token.call(
            abi.encodeWithSignature("approve(address,uint256)", spender, type(uint256).max)
        );
        require(ok);
    }

    function isValidSignature(bytes32 digest, bytes calldata sig) external view returns (bytes4) {
        (bytes32 r, bytes32 s) = abi.decode(sig[:64], (bytes32, bytes32));
        uint8 v = uint8(sig[64]);
        return ecrecover(digest, v, r, s) == owner ? bytes4(0x1626ba7e) : bytes4(0xffffffff);
    }
}
