// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {HashLock} from "../../src/HashLock.sol";

/// @dev Drives random initiate / redeem / refund / warp sequences against HashLock.
contract Handler is Test {
    HashLock public immutable htlc;
    bytes32[] public ids;
    mapping(bytes32 => bytes) public secretOf;

    uint256 public locked; // ETH that should still be held by the contract
    uint256 public paidOut;
    uint256 public deposited;

    address[3] internal actors = [address(0xA11CE), address(0xB0B), address(0xCA401)];

    constructor(HashLock _htlc) {
        htlc = _htlc;
    }

    function initiate(uint256 actorSeed, uint96 amount, uint32 lock, bytes32 seed) external {
        amount = uint96(bound(amount, 1, 100 ether));
        lock = uint32(bound(lock, 1, 30 days));
        address from = actors[actorSeed % 3];
        address to = actors[(actorSeed + 1) % 3];
        bytes memory secret = abi.encodePacked(seed);
        // hash before pranking: sha256 is a precompile call and would consume the prank
        bytes32 secretHash = sha256(secret);
        uint64 expiry = uint64(block.timestamp + lock);
        bytes32 expectedId =
            htlc.computeSwapId(from, to, address(0), amount, secretHash, expiry, seed);
        (address existing,,,,,,) = htlc.swaps(expectedId);
        if (existing != address(0)) return;

        vm.deal(from, amount);
        vm.prank(from);
        htlc.initiate{value: amount}(to, address(0), amount, secretHash, expiry, seed);
        ids.push(expectedId);
        secretOf[expectedId] = secret;
        locked += amount;
        deposited += amount;
    }

    function redeem(uint256 idx) external {
        if (ids.length == 0) return;
        bytes32 id = ids[idx % ids.length];
        (,, bool settled,,, uint256 amount,) = htlc.swaps(id);
        if (settled) return;
        htlc.redeem(id, secretOf[id]);
        locked -= amount;
        paidOut += amount;
    }

    function refund(uint256 idx) external {
        if (ids.length == 0) return;
        bytes32 id = ids[idx % ids.length];
        (, uint64 expiry, bool settled,,, uint256 amount,) = htlc.swaps(id);
        if (settled || block.timestamp < expiry) return;
        htlc.refund(id);
        locked -= amount;
        paidOut += amount;
    }

    function warp(uint32 secs) external {
        vm.warp(block.timestamp + bound(secs, 1, 7 days));
    }

    function count() external view returns (uint256) {
        return ids.length;
    }
}

contract HashLockInvariantTest is Test {
    HashLock internal htlc;
    Handler internal handler;

    function setUp() public {
        htlc = new HashLock();
        handler = new Handler(htlc);
        targetContract(address(handler));
    }

    /// Contract balance always equals the sum of unsettled swaps.
    function invariant_balanceMatchesOpenSwaps() public view {
        assertEq(address(htlc).balance, handler.locked());
    }

    /// Nothing is created or destroyed: deposits = still locked + paid out.
    function invariant_conservation() public view {
        assertEq(handler.deposited(), handler.locked() + handler.paidOut());
    }

    /// Settled swaps can never be settled again.
    function invariant_settledIsFinal() public {
        uint256 n = handler.count();
        for (uint256 i; i < n && i < 20; ++i) {
            bytes32 id = handler.ids(i);
            (,, bool settled,,,,) = htlc.swaps(id);
            if (!settled) continue;
            bytes memory secret = handler.secretOf(id);
            vm.expectRevert(HashLock.AlreadySettled.selector);
            htlc.redeem(id, secret);
        }
    }
}
