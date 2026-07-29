// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";
import {MockERC20} from "./mocks/MockERC20.sol";

/// @notice End-to-end swap across two simulated chains. Each "chain" is its own
///         HashLock deployment and calls run under the matching chain id, so swap
///         ids and EIP-712 domains differ exactly as they would on real networks.
///         Alice swaps ETH on chain A for Bob's USDC on chain B.
contract AtomicSwapTest is Test {
    uint256 internal constant CHAIN_A = 1;
    uint256 internal constant CHAIN_B = 8453;

    HashLock internal htlcA;
    HashLock internal htlcB;
    MockERC20 internal usdcB;

    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");

    bytes internal secret = "alice-only-knows-this-32-bytes!!";
    bytes32 internal secretHash;

    // initiator's leg must outlive the follower's, otherwise the initiator could
    // redeem on B and still refund on A.
    uint64 internal constant LOCK_A = 24 hours;
    uint64 internal constant LOCK_B = 12 hours;

    function setUp() public {
        secretHash = sha256(secret);

        htlcA = new HashLock();
        vm.deal(alice, 10 ether);

        htlcB = new HashLock();
        usdcB = new MockERC20("USD Coin", "USDC");
        usdcB.mint(bob, 30_000e18);
        vm.prank(bob);
        usdcB.approve(address(htlcB), type(uint256).max);
    }

    function test_happyPath() public {
        // 1. alice locks ETH on A for bob
        vm.chainId(CHAIN_A);
        vm.prank(alice);
        bytes32 idA = htlcA.initiate{value: 10 ether}(
            bob, address(0), 10 ether, secretHash, uint64(block.timestamp + LOCK_A), 0
        );

        // 2. bob sees the lock and mirrors it on B with a shorter timelock
        vm.chainId(CHAIN_B);
        vm.prank(bob);
        bytes32 idB = htlcB.initiate(
            alice, address(usdcB), 30_000e18, secretHash, uint64(block.timestamp + LOCK_B), 0
        );

        // 3. alice redeems on B, revealing the secret on-chain
        vm.recordLogs();
        vm.prank(alice);
        htlcB.redeem(idB, secret);
        bytes memory revealed = _secretFromLogs();
        assertEq(usdcB.balanceOf(alice), 30_000e18);

        // 4. bob reads the secret from B and redeems on A
        vm.chainId(CHAIN_A);
        vm.prank(bob);
        htlcA.redeem(idA, revealed);
        assertEq(bob.balance, 10 ether);
    }

    function test_followerNeverFunds_initiatorRefunds() public {
        vm.chainId(CHAIN_A);
        vm.prank(alice);
        bytes32 idA = htlcA.initiate{value: 10 ether}(
            bob, address(0), 10 ether, secretHash, uint64(block.timestamp + LOCK_A), 0
        );

        vm.warp(block.timestamp + LOCK_A);
        htlcA.refund(idA);
        assertEq(alice.balance, 10 ether);
    }

    function test_initiatorNeverReveals_bothRefund() public {
        vm.chainId(CHAIN_A);
        vm.prank(alice);
        bytes32 idA = htlcA.initiate{value: 10 ether}(
            bob, address(0), 10 ether, secretHash, uint64(block.timestamp + LOCK_A), 0
        );

        vm.chainId(CHAIN_B);
        vm.prank(bob);
        bytes32 idB = htlcB.initiate(
            alice, address(usdcB), 30_000e18, secretHash, uint64(block.timestamp + LOCK_B), 0
        );

        vm.warp(block.timestamp + LOCK_B);
        htlcB.refund(idB);
        assertEq(usdcB.balanceOf(bob), 30_000e18);

        vm.chainId(CHAIN_A);
        vm.warp(block.timestamp + LOCK_A);
        htlcA.refund(idA);
        assertEq(alice.balance, 10 ether);
    }

    function _secretFromLogs() internal returns (bytes memory) {
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; i++) {
            if (logs[i].topics[0] == HashLock.Redeemed.selector) {
                return abi.decode(logs[i].data, (bytes));
            }
        }
        revert("no Redeemed log");
    }
}
