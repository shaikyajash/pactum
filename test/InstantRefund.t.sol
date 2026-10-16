// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";
import {MockWallet} from "./mocks/MockWallet.sol";
import {Mock7702Delegate} from "./mocks/Mock7702Delegate.sol";

contract InstantRefundTest is Test {
    HashLock internal htlc;

    address internal alice = makeAddr("alice");
    uint256 internal bobKey = 0xB0B;
    address internal bob = vm.addr(bobKey);

    bytes32 internal secretHash = sha256("secret");
    bytes32 internal id;

    function setUp() public {
        htlc = new HashLock();
        vm.deal(alice, 10 ether);
        vm.prank(alice);
        id = htlc.initiate{value: 1 ether}(
            bob, address(0), 1 ether, secretHash, uint64(block.timestamp + 1 days), 0
        );
    }

    function _sign(uint256 key, bytes32 digest) internal pure returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    function test_instantRefund_withRedeemerSignature() public {
        bytes memory sig = _sign(bobKey, htlc.refundDigest(id));
        htlc.instantRefund(id, sig);
        assertEq(alice.balance, 10 ether);
    }

    function test_instantRefund_rejectsOtherSigner() public {
        bytes memory sig = _sign(0xBAD, htlc.refundDigest(id));
        vm.expectRevert(HashLock.InvalidSignature.selector);
        htlc.instantRefund(id, sig);
    }

    function test_instantRefund_signatureBoundToSwap() public {
        vm.prank(alice);
        bytes32 other = htlc.initiate{value: 1 ether}(
            bob, address(0), 1 ether, secretHash, uint64(block.timestamp + 1 days), bytes32("2")
        );
        bytes memory sig = _sign(bobKey, htlc.refundDigest(other));
        vm.expectRevert(HashLock.InvalidSignature.selector);
        htlc.instantRefund(id, sig);
    }

    function test_instantRefund_cannotReplay() public {
        bytes memory sig = _sign(bobKey, htlc.refundDigest(id));
        htlc.instantRefund(id, sig);
        vm.expectRevert(HashLock.AlreadySettled.selector);
        htlc.instantRefund(id, sig);
    }

    function test_instantRefund_rejectsMalleableSignature() public {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(bobKey, htlc.refundDigest(id));
        uint256 n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;
        bytes32 highS = bytes32(n - uint256(s));
        uint8 flippedV = v == 27 ? 28 : 27;
        vm.expectRevert(HashLock.InvalidSignature.selector);
        htlc.instantRefund(id, abi.encodePacked(r, highS, flippedV));
    }

    function test_instantRefund_rejectsBadLength() public {
        vm.expectRevert(HashLock.InvalidSignature.selector);
        htlc.instantRefund(id, hex"1234");
    }

    function _lockFor(address redeemer) internal returns (bytes32) {
        vm.prank(alice);
        return htlc.initiate{value: 1 ether}(
            redeemer, address(0), 1 ether, secretHash, uint64(block.timestamp + 1 days), "w"
        );
    }

    /// audit-1 L-3: a smart-wallet redeemer consents via EIP-1271.
    function test_instantRefund_contractRedeemerViaEip1271() public {
        MockWallet wallet = new MockWallet(bob);
        bytes32 wid = _lockFor(address(wallet));
        htlc.instantRefund(wid, _sign(bobKey, htlc.refundDigest(wid)));
        assertEq(alice.balance, 9 ether);
        (,, bool settled,,,,) = htlc.swaps(wid);
        assertTrue(settled);
    }

    function test_instantRefund_contractRedeemerRejectsWrongSigner() public {
        MockWallet wallet = new MockWallet(bob);
        bytes32 wid = _lockFor(address(wallet));
        bytes memory sig = _sign(0xBAD, htlc.refundDigest(wid));
        vm.expectRevert(HashLock.InvalidSignature.selector);
        htlc.instantRefund(wid, sig);
    }

    /// audit-1 I-1: an EIP-7702-delegated EOA has code whose delegate may not
    /// implement EIP-1271; its plain ECDSA signature must still be accepted.
    function test_instantRefund_7702DelegatedRedeemerFallsBackToEcdsa() public {
        vm.etch(bob, address(new Mock7702Delegate()).code);
        assertGt(bob.code.length, 0);

        bytes memory forged = _sign(0xBAD, htlc.refundDigest(id));
        vm.expectRevert(HashLock.InvalidSignature.selector);
        htlc.instantRefund(id, forged);

        htlc.instantRefund(id, _sign(bobKey, htlc.refundDigest(id)));
        assertEq(alice.balance, 10 ether);
    }

    function test_domainSeparator_chainBound() public {
        bytes32 before = htlc.domainSeparator();
        vm.chainId(8453);
        assertTrue(htlc.domainSeparator() != before);
    }
}
