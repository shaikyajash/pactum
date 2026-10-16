// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {PactumIntents} from "../../src/PactumIntents.sol";
import {MockERC20} from "../mocks/MockERC20.sol";
import {TreeBuilder} from "../MerkleSecrets.t.sol";

/// @dev Random resolvers attempt random partial fills of one 8-part order.
contract IntentsHandler is Test {
    PactumIntents public immutable intents;
    MockERC20 public immutable token;
    PactumIntents.Order internal order;
    bytes internal sig;
    bytes32[] internal hashes;

    uint256 public escrowed;
    uint256 public successfulFills;
    mapping(uint256 => bool) public indexUsed;
    bool public reusedIndex;

    constructor(PactumIntents _intents, MockERC20 _token, uint256 makerKey) {
        intents = _intents;
        token = _token;
        address maker = vm.addr(makerKey);

        for (uint256 i; i <= 8; ++i) {
            hashes.push(sha256(abi.encode(i)));
        }
        (bytes32 root,) = TreeBuilder.rootAndProof(hashes, 0);
        order = PactumIntents.Order({
            maker: maker,
            receiver: maker,
            srcToken: address(token),
            srcAmount: 1_000_000,
            dstChainId: 10,
            dstToken: address(0),
            startDstAmount: 2e18,
            endDstAmount: 1e18,
            auctionStart: uint64(block.timestamp),
            auctionEnd: uint64(block.timestamp + 1 hours),
            deadline: type(uint64).max,
            srcLock: 1 days,
            parts: 8,
            secretsRoot: root,
            nonce: 0,
            minDeposit: 0
        });
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(makerKey, intents.hashOrder(order));
        sig = abi.encodePacked(r, s, v);
    }

    function fill(uint256 resolverSeed, uint256 amount) external {
        (uint256 filled,,) = intents.fills(intents.hashOrder(order));
        uint256 remaining = order.srcAmount - filled;
        if (remaining == 0) return;
        amount = bound(amount, 1, remaining);
        uint256 idx = intents.secretIndexFor(order.srcAmount, order.parts, filled + amount);
        (, bytes32[] memory proof) = TreeBuilder.rootAndProof(hashes, idx);

        vm.prank(address(uint160(bound(resolverSeed, 1, 5))));
        try intents.fill(order, sig, amount, idx, hashes[idx], proof) {
            if (indexUsed[idx]) reusedIndex = true;
            indexUsed[idx] = true;
            escrowed += amount;
            successfulFills++;
        } catch {}
    }

    function filled() external view returns (uint256 f, uint32 next) {
        (f, next,) = intents.fills(intents.hashOrder(order));
    }

    function srcAmount() external view returns (uint256) {
        return order.srcAmount;
    }
}

contract PactumIntentsInvariantTest is Test {
    IntentsHandler internal handler;
    MockERC20 internal token;
    PactumIntents internal intents;

    function setUp() public {
        intents = new PactumIntents();
        token = new MockERC20("T", "T");
        uint256 makerKey = 0xA11CE;
        token.mint(vm.addr(makerKey), 1_000_000);
        vm.prank(vm.addr(makerKey));
        token.approve(address(intents), type(uint256).max);

        handler = new IntentsHandler(intents, token, makerKey);
        targetContract(address(handler));
    }

    /// guard against a vacuous pass: the handler swallows reverts
    function afterInvariant() public view {
        assertGt(handler.successfulFills(), 0);
    }

    function invariant_neverOverfilled() public view {
        (uint256 f,) = handler.filled();
        assertLe(f, handler.srcAmount());
    }

    function invariant_escrowMatchesFills() public view {
        (uint256 f,) = handler.filled();
        assertEq(f, handler.escrowed());
        assertEq(token.balanceOf(address(intents)), f);
    }

    function invariant_eachSecretUsedOnce() public view {
        assertFalse(handler.reusedIndex());
        (, uint32 next) = handler.filled();
        assertLe(next, 9);
        assertLe(handler.successfulFills(), 9);
    }
}
