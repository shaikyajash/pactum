// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";
import {SecretRelay} from "../src/ccip/SecretRelay.sol";
import {Client, IRouterClient} from "../src/ccip/Client.sol";
import {MockCCIPRouter} from "./mocks/MockCCIPRouter.sol";
import {MockERC20} from "./mocks/MockERC20.sol";

/// @notice Alice swaps ETH on chain A for the resolver's USDC on chain B.
///         After Alice redeems on B, a keeper relays the secret over CCIP and
///         the resolver is paid on A without ever touching chain A itself.
contract SecretRelayTest is Test {
    // real CCIP chain selectors for Ethereum and Base
    uint64 internal constant SEL_A = 5009297550715157269;
    uint64 internal constant SEL_B = 15971525489660198786;
    uint256 internal constant GAS = 300_000;

    MockCCIPRouter internal routerA;
    MockCCIPRouter internal routerB;
    HashLock internal htlcA;
    HashLock internal htlcB;
    SecretRelay internal relayA;
    SecretRelay internal relayB;
    MockERC20 internal usdcB;

    address internal owner = makeAddr("owner");
    address internal alice = makeAddr("alice");
    address internal resolver = makeAddr("resolver");
    address internal keeper = makeAddr("keeper");

    bytes internal secret = "relay-me-across-chains-32-bytes!";
    bytes32 internal secretHash;
    bytes32 internal idA;
    bytes32 internal idB;

    function setUp() public {
        secretHash = sha256(secret);

        routerA = new MockCCIPRouter(SEL_A);
        routerB = new MockCCIPRouter(SEL_B);
        routerA.connect(routerB);
        routerB.connect(routerA);

        htlcA = new HashLock();
        htlcB = new HashLock();
        relayA = new SecretRelay(IRouterClient(address(routerA)), htlcA, owner);
        relayB = new SecretRelay(IRouterClient(address(routerB)), htlcB, owner);

        vm.startPrank(owner);
        relayA.setPeer(SEL_B, address(relayB));
        relayB.setPeer(SEL_A, address(relayA));
        vm.stopPrank();

        usdcB = new MockERC20("USD Coin", "USDC");
        usdcB.mint(resolver, 3_000e18);
        vm.prank(resolver);
        usdcB.approve(address(htlcB), type(uint256).max);
        vm.deal(alice, 1 ether);
        vm.deal(keeper, 1 ether);

        vm.prank(alice);
        idA = htlcA.initiate{value: 1 ether}(
            resolver, address(0), 1 ether, secretHash, uint64(block.timestamp + 1 days), 0
        );
        vm.prank(resolver);
        idB = htlcB.initiate(
            alice, address(usdcB), 3_000e18, secretHash, uint64(block.timestamp + 12 hours), idA
        );
        vm.prank(alice);
        htlcB.redeem(idB, secret);
    }

    function _one(bytes32 id, bytes memory s)
        internal
        pure
        returns (bytes32[] memory ids, bytes[] memory secrets)
    {
        ids = new bytes32[](1);
        secrets = new bytes[](1);
        ids[0] = id;
        secrets[0] = s;
    }

    function _relay(bytes32[] memory ids, bytes[] memory secrets) internal {
        uint256 fee = routerB.FEE();
        vm.prank(keeper);
        relayB.relay{value: fee}(SEL_A, ids, secrets, GAS);
    }

    function _receive(SecretRelay relay, uint64 src, address sender, bytes memory data) internal {
        Client.Any2EVMMessage memory m = Client.Any2EVMMessage({
            messageId: bytes32(uint256(1)),
            sourceChainSelector: src,
            sender: abi.encode(sender),
            data: data,
            destTokenAmounts: new Client.EVMTokenAmount[](0)
        });
        relay.ccipReceive(m);
    }

    function test_relay_redeemsRemoteLeg() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        _relay(ids, secrets);

        assertEq(resolver.balance, 0, "nothing paid before delivery");
        assertEq(routerA.pending(), 1);

        vm.expectEmit(address(relayA));
        emit SecretRelay.SecretRelayed(idA, SEL_B);
        routerA.deliverNext();

        assertEq(resolver.balance, 1 ether);
        (,, bool settled,,,,) = htlcA.swaps(idA);
        assertTrue(settled);
    }

    function test_relay_refundsExcessFee() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        vm.prank(keeper);
        relayB.relay{value: 0.01 ether}(SEL_A, ids, secrets, GAS);
        assertEq(keeper.balance, 1 ether - routerB.FEE());
    }

    function test_relay_revertsOnInsufficientFee() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        uint256 fee = routerB.FEE();
        vm.prank(keeper);
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.InsufficientFee.selector, fee, fee - 1));
        relayB.relay{value: fee - 1}(SEL_A, ids, secrets, GAS);
    }

    function test_relay_quoteMatchesRouter() public view {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        assertEq(relayB.quote(SEL_A, ids, secrets, GAS), routerB.FEE());
    }

    function test_relay_wrongSecretIsLoggedNotReverted() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, "nope");
        _relay(ids, secrets);

        vm.recordLogs();
        routerA.deliverNext();
        Vm.Log[] memory logs = vm.getRecordedLogs();
        assertEq(logs[0].topics[0], SecretRelay.RelayFailed.selector);
        assertEq(resolver.balance, 0);
    }

    function test_relay_alreadyRedeemedDoesNotBlockBatch() public {
        vm.deal(alice, 1 ether);
        vm.prank(alice);
        bytes32 idA2 = htlcA.initiate{value: 1 ether}(
            resolver, address(0), 1 ether, secretHash, uint64(block.timestamp + 1 days), "2"
        );
        htlcA.redeem(idA, secret); // resolver already claimed by hand

        bytes32[] memory ids = new bytes32[](2);
        bytes[] memory secrets = new bytes[](2);
        (ids[0], ids[1]) = (idA, idA2);
        (secrets[0], secrets[1]) = (secret, secret);
        _relay(ids, secrets);
        routerA.deliverNext();

        assertEq(resolver.balance, 2 ether);
    }

    function test_ccipReceive_onlyRouter() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        vm.expectRevert(SecretRelay.OnlyRouter.selector);
        _receive(relayA, SEL_B, address(relayB), abi.encode(ids, secrets));
    }

    function test_ccipReceive_rejectsUntrustedPeer() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        address rogue = makeAddr("rogue");
        vm.prank(address(routerA));
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.UntrustedPeer.selector, SEL_B, rogue));
        _receive(relayA, SEL_B, rogue, abi.encode(ids, secrets));
    }

    function test_ccipReceive_rejectsTrustedSenderFromWrongChain() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        vm.prank(address(routerA));
        vm.expectRevert(
            abi.encodeWithSelector(SecretRelay.UntrustedPeer.selector, uint64(1), address(relayB))
        );
        _receive(relayA, 1, address(relayB), abi.encode(ids, secrets));
    }

    function test_relay_revertsWithoutPeer() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        uint256 fee = routerB.FEE();
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.NoPeer.selector, 999));
        relayB.relay{value: fee}(999, ids, secrets, GAS);
    }

    function test_relay_validatesBatchShape() public {
        bytes32[] memory ids = new bytes32[](2);
        bytes[] memory secrets = new bytes[](1);
        uint256 fee = routerB.FEE();
        vm.expectRevert(SecretRelay.LengthMismatch.selector);
        relayB.relay{value: fee}(SEL_A, ids, secrets, GAS);

        vm.expectRevert(SecretRelay.EmptyBatch.selector);
        relayB.relay{value: fee}(SEL_A, new bytes32[](0), new bytes[](0), GAS);
    }

    function test_admin_onlyOwner() public {
        vm.expectRevert(SecretRelay.NotOwner.selector);
        relayA.setPeer(1, address(1));

        vm.prank(owner);
        relayA.transferOwnership(alice);
        assertEq(relayA.owner(), alice);
    }

    function test_supportsReceiverInterface() public view {
        assertTrue(relayA.supportsInterface(0x85572ffb)); // ccipReceive selector
        assertTrue(relayA.supportsInterface(0x01ffc9a7)); // ERC-165
        assertFalse(relayA.supportsInterface(0xdeadbeef));
    }

    /// Safety property: whatever a (compromised) peer sends, funds only ever
    /// reach the swap's fixed redeemer, and only with the true preimage.
    function testFuzz_forgedMessagesCannotMisdirectFunds(bytes calldata forged) public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, forged);

        vm.prank(address(routerA));
        _receive(relayA, SEL_B, address(relayB), abi.encode(ids, secrets));

        if (sha256(forged) == secretHash) assertEq(resolver.balance, 1 ether);
        else assertEq(resolver.balance, 0);
        assertEq(alice.balance + resolver.balance + address(htlcA).balance, 1 ether);
    }
}
