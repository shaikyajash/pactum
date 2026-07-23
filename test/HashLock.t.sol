// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";
import {SafeTransfer} from "../src/lib/SafeTransfer.sol";
import {MockERC20, NoReturnERC20} from "./mocks/MockERC20.sol";

contract HashLockTest is Test {
    HashLock internal htlc;
    MockERC20 internal usdc;
    NoReturnERC20 internal usdt;

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

        usdc = new MockERC20("USD Coin", "USDC");
        usdt = new NoReturnERC20();
        usdc.mint(alice, 1_000e18);
        usdt.mint(alice, 1_000e18);
        vm.startPrank(alice);
        usdc.approve(address(htlc), type(uint256).max);
        usdt.approve(address(htlc), type(uint256).max);
        vm.stopPrank();
    }

    function _initiate(uint256 amount) internal returns (bytes32) {
        vm.prank(alice);
        return htlc.initiate{value: amount}(bob, address(0), amount, secretHash, expiry, 0);
    }

    function _initiateToken(address token, uint256 amount) internal returns (bytes32) {
        vm.prank(alice);
        return htlc.initiate(bob, token, amount, secretHash, expiry, 0);
    }

    function test_initiate_storesSwap() public {
        bytes32 id = _initiate(1 ether);

        (address init, uint64 exp, bool settled, address red, address tok, uint256 amt, bytes32 h)
        = htlc.swaps(id);
        assertEq(tok, address(0));
        assertEq(init, alice);
        assertEq(red, bob);
        assertEq(amt, 1 ether);
        assertEq(exp, expiry);
        assertEq(h, secretHash);
        assertFalse(settled);
        assertEq(address(htlc).balance, 1 ether);
    }

    function test_initiate_emitsEvent() public {
        bytes32 id = htlc.computeSwapId(alice, bob, address(0), 1 ether, secretHash, expiry, 0);
        vm.expectEmit(address(htlc));
        emit HashLock.Initiated(id, alice, bob, address(0), 1 ether, secretHash, expiry);
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
        htlc.initiate{value: 1}(address(0), address(0), 1, secretHash, expiry, 0);

        vm.expectRevert(HashLock.ZeroAmount.selector);
        htlc.initiate(bob, address(0), 0, secretHash, expiry, 0);

        vm.expectRevert(HashLock.InvalidExpiry.selector);
        htlc.initiate{value: 1}(bob, address(0), 1, secretHash, uint64(block.timestamp), 0);

        vm.expectRevert(HashLock.InvalidValue.selector);
        htlc.initiate{value: 1}(bob, address(0), 2, secretHash, expiry, 0);

        vm.expectRevert(HashLock.InvalidValue.selector);
        htlc.initiate{value: 1}(bob, address(usdc), 1, secretHash, expiry, 0);
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

    // ---------------------------------------------------------------- ERC20

    function test_erc20_redeem() public {
        bytes32 id = _initiateToken(address(usdc), 250e18);
        assertEq(usdc.balanceOf(address(htlc)), 250e18);

        htlc.redeem(id, secret);
        assertEq(usdc.balanceOf(bob), 250e18);
        assertEq(usdc.balanceOf(address(htlc)), 0);
    }

    function test_erc20_refund() public {
        bytes32 id = _initiateToken(address(usdc), 250e18);
        vm.warp(expiry);
        htlc.refund(id);
        assertEq(usdc.balanceOf(alice), 1_000e18);
    }

    function test_erc20_noReturnToken() public {
        bytes32 id = _initiateToken(address(usdt), 10e18);
        htlc.redeem(id, secret);
        assertEq(usdt.balanceOf(bob), 10e18);
    }

    function test_erc20_revertsOnNonContractToken() public {
        vm.prank(alice);
        vm.expectRevert(SafeTransfer.TokenTransferFailed.selector);
        htlc.initiate(bob, makeAddr("eoa"), 1, secretHash, expiry, 0);
    }

    function test_erc20_revertsWithoutAllowance() public {
        vm.prank(alice);
        usdc.approve(address(htlc), 0);
        vm.prank(alice);
        vm.expectRevert(SafeTransfer.TokenTransferFailed.selector);
        htlc.initiate(bob, address(usdc), 1, secretHash, expiry, 0);
    }

    // ---------------------------------------------------------------- fuzz

    function testFuzz_redeemOnlyWithPreimage(bytes calldata guess) public {
        bytes32 id = _initiate(1 ether);
        if (sha256(guess) != secretHash) {
            vm.expectRevert(HashLock.InvalidSecret.selector);
        }
        htlc.redeem(id, guess);
    }

    function testFuzz_saltSeparatesSwaps(bytes32 saltA, bytes32 saltB) public {
        vm.assume(saltA != saltB);
        vm.startPrank(alice);
        bytes32 a = htlc.initiate{value: 1}(bob, address(0), 1, secretHash, expiry, saltA);
        bytes32 b = htlc.initiate{value: 1}(bob, address(0), 1, secretHash, expiry, saltB);
        vm.stopPrank();
        assertTrue(a != b);
    }

    function testFuzz_refundTiming(uint64 delay) public {
        bytes32 id = _initiate(1 ether);
        delay = uint64(bound(delay, 0, 365 days));
        vm.warp(block.timestamp + delay);
        if (block.timestamp < expiry) vm.expectRevert(HashLock.NotExpired.selector);
        htlc.refund(id);
    }
}
