// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @notice Merkle commitments to an ordered list of secret hashes.
/// @dev A maker that wants an order to be fillable in N parts commits to N+1
///      secrets up front. Leaf `i` binds the index to the hash so a proof for
///      secret 3 can never be presented as secret 1. Pairs are hashed in sorted
///      order (same convention as OpenZeppelin's MerkleProof), so proofs carry
///      no direction bits.
library MerkleSecrets {
    function leaf(uint256 index, bytes32 secretHash) internal pure returns (bytes32) {
        // double hash so a leaf can never be confused with an inner node
        return keccak256(bytes.concat(keccak256(abi.encode(index, secretHash))));
    }

    function verify(bytes32 root, uint256 index, bytes32 secretHash, bytes32[] calldata proof)
        internal
        pure
        returns (bool)
    {
        bytes32 node = leaf(index, secretHash);
        for (uint256 i; i < proof.length; ++i) {
            node = _hashPair(node, proof[i]);
        }
        return node == root;
    }

    function _hashPair(bytes32 a, bytes32 b) private pure returns (bytes32) {
        return a < b ? keccak256(abi.encode(a, b)) : keccak256(abi.encode(b, a));
    }
}
