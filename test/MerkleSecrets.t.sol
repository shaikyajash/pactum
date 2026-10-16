// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {MerkleSecrets} from "../src/lib/MerkleSecrets.sol";

contract MerkleHarness {
    function verify(bytes32 root, uint256 i, bytes32 h, bytes32[] calldata proof)
        external
        pure
        returns (bool)
    {
        return MerkleSecrets.verify(root, i, h, proof);
    }
}

/// @dev Reference tree builder for tests; mirrors resolver/src/merkle.ts.
library TreeBuilder {
    function _pair(bytes32 a, bytes32 b) private pure returns (bytes32) {
        return a < b ? keccak256(abi.encode(a, b)) : keccak256(abi.encode(b, a));
    }

    /// Builds a tree over `hashes`, duplicating the last node on odd levels.
    function rootAndProof(bytes32[] memory hashes, uint256 target)
        internal
        pure
        returns (bytes32 root, bytes32[] memory proof)
    {
        bytes32[] memory level = new bytes32[](hashes.length);
        for (uint256 i; i < hashes.length; ++i) {
            level[i] = MerkleSecrets.leaf(i, hashes[i]);
        }
        bytes32[] memory tmp = new bytes32[](64);
        uint256 depth;
        uint256 idx = target;
        while (level.length > 1) {
            uint256 sib = idx ^ 1;
            tmp[depth++] = sib < level.length ? level[sib] : level[idx];
            bytes32[] memory next = new bytes32[]((level.length + 1) / 2);
            for (uint256 i; i < next.length; ++i) {
                bytes32 l = level[2 * i];
                bytes32 r = 2 * i + 1 < level.length ? level[2 * i + 1] : l;
                next[i] = _pair(l, r);
            }
            level = next;
            idx /= 2;
        }
        root = level[0];
        proof = new bytes32[](depth);
        for (uint256 i; i < depth; ++i) {
            proof[i] = tmp[i];
        }
    }
}

contract MerkleSecretsTest is Test {
    MerkleHarness internal m = new MerkleHarness();

    function _hashes(uint256 n) internal pure returns (bytes32[] memory h) {
        h = new bytes32[](n);
        for (uint256 i; i < n; ++i) {
            h[i] = sha256(abi.encode("secret", i));
        }
    }

    function test_singleLeaf() public view {
        bytes32[] memory h = _hashes(1);
        (bytes32 root, bytes32[] memory proof) = TreeBuilder.rootAndProof(h, 0);
        assertEq(proof.length, 0);
        assertTrue(m.verify(root, 0, h[0], proof));
    }

    function test_everyLeafVerifies() public view {
        bytes32[] memory h = _hashes(11);
        for (uint256 i; i < h.length; ++i) {
            (bytes32 root, bytes32[] memory proof) = TreeBuilder.rootAndProof(h, i);
            assertTrue(m.verify(root, i, h[i], proof));
        }
    }

    function test_indexIsBound() public view {
        bytes32[] memory h = _hashes(4);
        (bytes32 root, bytes32[] memory proof) = TreeBuilder.rootAndProof(h, 2);
        assertFalse(m.verify(root, 1, h[2], proof), "same hash, wrong index");
        assertFalse(m.verify(root, 2, h[1], proof), "right index, wrong hash");
    }

    function testFuzz_onlyCommittedSecretsVerify(uint8 n, uint8 target, bytes32 forged)
        public
        view
    {
        n = uint8(bound(n, 1, 40));
        target = uint8(bound(target, 0, n - 1));
        bytes32[] memory h = _hashes(n);
        (bytes32 root, bytes32[] memory proof) = TreeBuilder.rootAndProof(h, target);
        assertTrue(m.verify(root, target, h[target], proof));
        if (forged != h[target]) assertFalse(m.verify(root, target, forged, proof));
    }
}
