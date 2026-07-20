// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";

contract HashLockTest is Test {
    HashLock internal htlc;

    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal relayer = makeAddr("relayer");

    bytes internal secret = abi.encodePacked(keccak256("super secret"));
    bytes32 internal secretHash;
    uint64 internal expiry;

    function setUp() public {
        htlc = new HashLock();
        secretHash = sha256(secret);
        expiry = uint64(block.timestamp + 1 hours);
        vm.deal(alice, 100 ether);
    }

    function _initiate(uint256 amount) internal returns (bytes32) {
        vm.prank(alice);
        return htlc.initiate{value: amount}(bob, secretHash, expiry, bytes32(0));
    }

    function test_initiate_storesSwap() public {
        bytes32 id = _initiate(1 ether);

        (address init, uint64 exp, bool settled, address red, uint256 amt, bytes32 h) =
            htlc.swaps(id);
        assertEq(init, alice);
        assertEq(red, bob);
        assertEq(amt, 1 ether);
        assertEq(exp, expiry);
        assertEq(h, secretHash);
        assertFalse(settled);
        assertEq(address(htlc).balance, 1 ether);
    }

    function test_initiate_emitsEvent() public {
        bytes32 id = htlc.computeSwapId(alice, bob, secretHash, 1 ether, expiry, bytes32(0));
        vm.expectEmit(address(htlc));
        emit HashLock.Initiated(id, alice, bob, secretHash, 1 ether, expiry);
        _initiate(1 ether);
    }

    function test_initiate_revertsOnDuplicate() public {
        _initiate(1 ether);
        vm.expectRevert(HashLock.SwapExists.selector);
        _initiate(1 ether);
    }

    function test_initiate_revertsOnBadParams() public {
        vm.startPrank(alice);
        vm.expectRevert(HashLock.ZeroAddress.selector);
        htlc.initiate{value: 1}(address(0), secretHash, expiry, 0);

        vm.expectRevert(HashLock.ZeroAmount.selector);
        htlc.initiate(bob, secretHash, expiry, 0);

        vm.expectRevert(HashLock.InvalidExpiry.selector);
        htlc.initiate{value: 1}(bob, secretHash, uint64(block.timestamp), 0);
        vm.stopPrank();
    }

    function test_redeem_paysRedeemer() public {
        bytes32 id = _initiate(1 ether);

        // a third party can submit the secret; funds still go to bob
        vm.prank(relayer);
        htlc.redeem(id, secret);

        assertEq(bob.balance, 1 ether);
        assertEq(relayer.balance, 0);
        assertEq(address(htlc).balance, 0);
    }

    function test_redeem_emitsSecret() public {
        bytes32 id = _initiate(1 ether);
        vm.expectEmit(address(htlc));
        emit HashLock.Redeemed(id, secretHash, secret);
        htlc.redeem(id, secret);
    }

    function test_redeem_revertsOnWrongSecret() public {
        bytes32 id = _initiate(1 ether);
        vm.expectRevert(HashLock.InvalidSecret.selector);
        htlc.redeem(id, "wrong");
    }

    function test_redeem_revertsTwice() public {
        bytes32 id = _initiate(1 ether);
        htlc.redeem(id, secret);
        vm.expectRevert(HashLock.AlreadySettled.selector);
        htlc.redeem(id, secret);
    }

    function test_redeem_worksAfterExpiryUntilRefunded() public {
        bytes32 id = _initiate(1 ether);
        vm.warp(expiry + 1);
        htlc.redeem(id, secret);
        assertEq(bob.balance, 1 ether);
    }

    function test_refund_afterExpiry() public {
        bytes32 id = _initiate(1 ether);
        vm.warp(expiry);
        htlc.refund(id);
        assertEq(alice.balance, 100 ether);
    }

    function test_refund_revertsBeforeExpiry() public {
        bytes32 id = _initiate(1 ether);
        vm.expectRevert(HashLock.NotExpired.selector);
        htlc.refund(id);
    }

    function test_refund_blocksLaterRedeem() public {
        bytes32 id = _initiate(1 ether);
        vm.warp(expiry);
        htlc.refund(id);
        vm.expectRevert(HashLock.AlreadySettled.selector);
        htlc.redeem(id, secret);
    }

    function test_unknownSwap_reverts() public {
        vm.expectRevert(HashLock.SwapNotFound.selector);
        htlc.redeem(bytes32(uint256(1)), secret);
        vm.expectRevert(HashLock.SwapNotFound.selector);
        htlc.refund(bytes32(uint256(1)));
    }
}
