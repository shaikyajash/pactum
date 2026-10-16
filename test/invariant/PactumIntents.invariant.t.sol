// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {PactumIntents} from "../../src/PactumIntents.sol";
import {MockERC20} from "../mocks/MockERC20.sol";
import {TreeBuilder} from "../MerkleSecrets.t.sol";

/// @dev Random resolvers attempt random partial fills of one 8-part order, posting
///      random deposits, while escrows are redeemed, refunded by keepers, native
///      swaps are opened alongside, and credited deposits are withdrawn.
contract IntentsHandler is Test {
    PactumIntents public immutable intents;
    MockERC20 public immutable token;
    PactumIntents.Order internal order;
    bytes internal sig;
    bytes32[] internal hashes;

    uint256 public escrowed; // ERC20 still locked in open escrows
    uint256 public totalFilled;
    uint256 public openDeposits; // deposits of unsettled escrows
    uint256 public nativeLocked; // native swap amounts of unsettled escrows
    uint256 public credited;
    uint256 public withdrawn;
    uint256 public successfulFills;
    uint256 public settlements;

    bytes32[] public ids;
    mapping(bytes32 => bytes) internal secretOf;
    address[] internal actors;
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
            receiver: bytes32(uint256(uint160(maker))),
            srcToken: address(token),
            srcAmount: 1_000_000,
            dstChainId: 10,
            dstToken: bytes32(0),
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

        actors.push(maker);
        for (uint256 i; i < 8; ++i) {
            actors.push(address(uint160(0x1000 + i))); // 0..4 resolvers, 5..7 keepers
        }
    }

    function _resolver(uint256 seed) internal view returns (address) {
        return actors[1 + seed % 5];
    }

    function fill(uint256 resolverSeed, uint256 amount, uint256 deposit) external {
        (uint256 filled,,) = intents.fills(intents.hashOrder(order));
        uint256 remaining = order.srcAmount - filled;
        if (remaining == 0) return;
        amount = bound(amount, 1, remaining);
        uint256 idx = intents.secretIndexFor(order.srcAmount, order.parts, filled + amount);
        (, bytes32[] memory proof) = TreeBuilder.rootAndProof(hashes, idx);

        deposit = bound(deposit, 0, 1 ether);
        address resolver = _resolver(resolverSeed);
        vm.deal(resolver, deposit);
        vm.prank(resolver);
        try intents.fill{value: deposit}(order, sig, amount, idx, hashes[idx], proof) returns (
            bytes32 id
        ) {
            if (indexUsed[idx]) reusedIndex = true;
            indexUsed[idx] = true;
            escrowed += amount;
            totalFilled += amount;
            openDeposits += deposit;
            successfulFills++;
            ids.push(id);
            secretOf[id] = abi.encode(idx);
        } catch {}
    }

    /// native HashLock swap between two actors, next to the intents escrows
    function initiateNative(uint256 fromSeed, uint256 toSeed, uint96 amount, bytes32 salt)
        external
    {
        amount = uint96(bound(amount, 1, 10 ether));
        address from = actors[fromSeed % actors.length];
        address to = actors[toSeed % actors.length];
        bytes memory secret = abi.encode(salt);
        bytes32 h = sha256(secret);
        vm.deal(from, amount);
        vm.prank(from);
        try intents.initiate{value: amount}(
            to, address(0), amount, h, uint64(block.timestamp + 1 days), salt
        ) returns (
            bytes32 id
        ) {
            nativeLocked += amount;
            ids.push(id);
            secretOf[id] = secret;
        } catch {}
    }

    function redeem(uint256 idx, uint256 callerSeed) external {
        if (ids.length == 0) return;
        bytes32 id = ids[idx % ids.length];
        (,, bool settled,, address token, uint256 amount,) = intents.swaps(id);
        if (settled) return;
        uint256 dep = intents.deposits(id);
        vm.prank(actors[callerSeed % actors.length]);
        intents.redeem(id, secretOf[id]);
        _settled(token, amount, dep);
    }

    function refund(uint256 idx, uint256 callerSeed) external {
        if (ids.length == 0) return;
        bytes32 id = ids[idx % ids.length];
        (, uint64 expiry, bool settled,, address token, uint256 amount,) = intents.swaps(id);
        if (settled || block.timestamp < expiry) return;
        uint256 dep = intents.deposits(id);
        vm.prank(actors[callerSeed % actors.length]);
        intents.refund(id);
        _settled(token, amount, dep);
    }

    function withdraw(uint256 actorSeed) external {
        address a = actors[actorSeed % actors.length];
        uint256 c = intents.depositCredit(a);
        if (c == 0) return;
        vm.prank(a);
        intents.withdrawDeposits();
        withdrawn += c;
    }

    function warp(uint32 secs) external {
        vm.warp(block.timestamp + bound(secs, 1, 2 days));
    }

    function _settled(address token, uint256 amount, uint256 dep) internal {
        if (token == address(0)) nativeLocked -= amount;
        else escrowed -= amount;
        openDeposits -= dep;
        credited += dep;
        settlements++;
    }

    function sumCredits() external view returns (uint256 sum) {
        for (uint256 i; i < actors.length; ++i) {
            sum += intents.depositCredit(actors[i]);
        }
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
        assertEq(f, handler.totalFilled());
        assertEq(token.balanceOf(address(intents)), handler.escrowed());
    }

    /// The contract's ETH is exactly open deposits + credited-but-unwithdrawn
    /// deposits + open native escrows.
    function invariant_nativeBalanceFullyBacked() public view {
        uint256 credits = handler.sumCredits();
        assertEq(credits, handler.credited() - handler.withdrawn(), "credits match ledger");
        assertEq(
            address(intents).balance,
            handler.openDeposits() + credits + handler.nativeLocked(),
            "balance == open deposits + credits + native escrow"
        );
    }

    function invariant_eachSecretUsedOnce() public view {
        assertFalse(handler.reusedIndex());
        (, uint32 next) = handler.filled();
        assertLe(next, 9);
        assertLe(handler.successfulFills(), 9);
    }
}
