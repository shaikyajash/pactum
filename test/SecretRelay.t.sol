// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";
import {SecretRelay} from "../src/layerzero/SecretRelay.sol";
import {Origin} from "../src/layerzero/ILayerZeroEndpointV2.sol";
import {MockEndpoint} from "./mocks/MockEndpoint.sol";
import {MockERC20} from "./mocks/MockERC20.sol";

/// @notice Alice swaps ETH on chain A for the resolver's USDC on chain B.
///         After Alice redeems on B, a keeper relays the secret over LayerZero
///         and the resolver is paid on A without ever touching chain A itself.
contract SecretRelayTest is Test {
    uint32 internal constant EID_A = 30101; // ethereum
    uint32 internal constant EID_B = 30184; // base

    MockEndpoint internal lzA;
    MockEndpoint internal lzB;
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

        lzA = new MockEndpoint(EID_A);
        lzB = new MockEndpoint(EID_B);
        lzA.connect(lzB);
        lzB.connect(lzA);

        htlcA = new HashLock();
        htlcB = new HashLock();
        relayA = new SecretRelay(address(lzA), htlcA, owner);
        relayB = new SecretRelay(address(lzB), htlcB, owner);

        vm.startPrank(owner);
        relayA.setPeer(EID_B, _b32(address(relayB)));
        relayB.setPeer(EID_A, _b32(address(relayA)));
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

    function _b32(address a) internal pure returns (bytes32) {
        return bytes32(uint256(uint160(a)));
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
        vm.prank(keeper);
        relayB.relay{value: lzB.FEE()}(EID_A, ids, secrets, "");
    }

    function test_relay_redeemsRemoteLeg() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        _relay(ids, secrets);

        assertEq(resolver.balance, 0, "nothing paid before delivery");
        assertEq(lzA.pending(), 1);

        vm.expectEmit(address(relayA));
        emit SecretRelay.SecretRelayed(idA, EID_B);
        lzA.deliverNext();

        assertEq(resolver.balance, 1 ether);
        (,, bool settled,,,,) = htlcA.swaps(idA);
        assertTrue(settled);
    }

    function test_relay_keeperPaysFeeAndIsRefunded() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        vm.prank(keeper);
        relayB.relay{value: 0.01 ether}(EID_A, ids, secrets, "");
        assertEq(keeper.balance, 1 ether - lzB.FEE());
    }

    function test_relay_wrongSecretIsLoggedNotReverted() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, "nope");
        _relay(ids, secrets);

        vm.recordLogs();
        lzA.deliverNext();
        Vm.Log[] memory logs = vm.getRecordedLogs();
        assertEq(logs[0].topics[0], SecretRelay.RelayFailed.selector);
        assertEq(resolver.balance, 0);
    }

    function test_relay_alreadyRedeemedDoesNotBlockBatch() public {
        // second swap so the batch has one stale and one live entry
        vm.prank(alice);
        vm.deal(alice, 1 ether);
        bytes32 idA2 = htlcA.initiate{value: 1 ether}(
            resolver, address(0), 1 ether, secretHash, uint64(block.timestamp + 1 days), "2"
        );
        htlcA.redeem(idA, secret); // resolver already claimed by hand

        bytes32[] memory ids = new bytes32[](2);
        bytes[] memory secrets = new bytes[](2);
        (ids[0], ids[1]) = (idA, idA2);
        (secrets[0], secrets[1]) = (secret, secret);
        _relay(ids, secrets);
        lzA.deliverNext();

        assertEq(resolver.balance, 2 ether);
    }

    function test_lzReceive_onlyEndpoint() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        vm.expectRevert(SecretRelay.OnlyEndpoint.selector);
        relayA.lzReceive(
            Origin(EID_B, _b32(address(relayB)), 1), 0, abi.encode(ids, secrets), address(0), ""
        );
    }

    function test_lzReceive_rejectsUntrustedPeer() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        bytes32 rogue = _b32(makeAddr("rogue"));
        vm.prank(address(lzA));
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.UntrustedPeer.selector, EID_B, rogue));
        relayA.lzReceive(Origin(EID_B, rogue, 1), 0, abi.encode(ids, secrets), address(0), "");
    }

    function test_relay_revertsWithoutPeer() public {
        (bytes32[] memory ids, bytes[] memory secrets) = _one(idA, secret);
        uint256 fee = lzB.FEE();
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.NoPeer.selector, 999));
        relayB.relay{value: fee}(999, ids, secrets, "");
    }

    function test_relay_validatesBatchShape() public {
        bytes32[] memory ids = new bytes32[](2);
        bytes[] memory secrets = new bytes[](1);
        uint256 fee = lzB.FEE();
        vm.expectRevert(SecretRelay.LengthMismatch.selector);
        relayB.relay{value: fee}(EID_A, ids, secrets, "");

        vm.expectRevert(SecretRelay.EmptyBatch.selector);
        relayB.relay{value: fee}(EID_A, new bytes32[](0), new bytes[](0), "");
    }

    function test_admin_onlyOwner() public {
        vm.expectRevert(SecretRelay.NotOwner.selector);
        relayA.setPeer(1, bytes32(uint256(1)));

        vm.prank(owner);
        relayA.transferOwnership(alice);
        assertEq(relayA.owner(), alice);
        assertEq(lzA.delegates(address(relayA)), alice);
    }

    function test_allowInitializePath() public view {
        assertTrue(relayA.allowInitializePath(Origin(EID_B, _b32(address(relayB)), 0)));
        assertFalse(relayA.allowInitializePath(Origin(EID_B, _b32(alice), 0)));
    }

    /// Safety property: whatever a (compromised) peer sends, funds only ever
    /// reach the swap's fixed redeemer, and only with the true preimage.
    function testFuzz_forgedMessagesCannotMisdirectFunds(bytes calldata forged) public {
        bytes32[] memory ids = new bytes32[](1);
        bytes[] memory secrets = new bytes[](1);
        ids[0] = idA;
        secrets[0] = forged;

        vm.prank(address(lzA));
        relayA.lzReceive(
            Origin(EID_B, _b32(address(relayB)), 1), 0, abi.encode(ids, secrets), address(0), ""
        );

        if (sha256(forged) == secretHash) assertEq(resolver.balance, 1 ether);
        else assertEq(resolver.balance, 0);
        assertEq(alice.balance + resolver.balance + address(htlcA).balance, 1 ether);
    }
}
