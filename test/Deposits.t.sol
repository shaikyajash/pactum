// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {PactumIntents} from "../src/PactumIntents.sol";
import {SecretRelay} from "../src/ccip/SecretRelay.sol";
import {Client, IRouterClient} from "../src/ccip/Client.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockCCIPRouter} from "./mocks/MockCCIPRouter.sol";

/// @dev A redeemer contract that refuses plain ETH transfers.
contract EthRejecter {
    receive() external payable {
        revert("no thanks");
    }

    function claimFrom(SecretRelay relay) external {
        relay.claim();
    }
}

contract DepositsTest is Test {
    PactumIntents internal intents;
    MockERC20 internal usdc;

    uint256 internal makerKey = 0xA11CE;
    address internal maker = vm.addr(makerKey);
    address internal resolver = makeAddr("resolver");
    address internal keeper = makeAddr("keeper");

    uint256 internal constant SRC = 100e18;
    uint256 internal constant DEPOSIT = 0.01 ether;
    bytes internal secret = "deposit-test-secret";
    bytes32 internal secretHash;

    function setUp() public {
        vm.warp(1_800_000_000);
        intents = new PactumIntents();
        usdc = new MockERC20("USD Coin", "USDC");
        usdc.mint(maker, SRC);
        vm.prank(maker);
        usdc.approve(address(intents), type(uint256).max);
        vm.deal(resolver, 1 ether);
        secretHash = sha256(secret);
    }

    function _order(address receiver) internal view returns (PactumIntents.Order memory) {
        bytes32 root = keccak256(bytes.concat(keccak256(abi.encode(uint256(0), secretHash))));
        return PactumIntents.Order({
            maker: maker,
            receiver: receiver,
            srcToken: address(usdc),
            srcAmount: SRC,
            dstChainId: 8453,
            dstToken: address(0),
            startDstAmount: 1 ether,
            endDstAmount: 1 ether,
            auctionStart: uint64(block.timestamp),
            auctionEnd: uint64(block.timestamp),
            deadline: uint64(block.timestamp + 1 hours),
            srcLock: 6 hours,
            parts: 1,
            secretsRoot: root,
            nonce: 1,
            minDeposit: DEPOSIT
        });
    }

    function _fill(address from, uint256 deposit) internal returns (bytes32) {
        PactumIntents.Order memory o = _order(maker);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(makerKey, intents.hashOrder(o));
        vm.prank(from);
        return intents.fill{value: deposit}(
            o, abi.encodePacked(r, s, v), SRC, 0, secretHash, new bytes32[](0)
        );
    }

    function test_depositBelowMinimumReverts() public {
        PactumIntents.Order memory o = _order(maker);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(makerKey, intents.hashOrder(o));
        bytes memory sig = abi.encodePacked(r, s, v);
        vm.prank(resolver);
        vm.expectRevert(PactumIntents.DepositTooLow.selector);
        intents.fill{value: DEPOSIT - 1}(o, sig, SRC, 0, secretHash, new bytes32[](0));
    }

    function test_depositHeldWithEscrow() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        assertEq(intents.deposits(id), DEPOSIT);
        assertEq(address(intents).balance, DEPOSIT);
    }

    function test_resolverRedeemReclaimsDeposit() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        vm.prank(resolver);
        intents.redeem(id, secret);

        assertEq(usdc.balanceOf(resolver), SRC);
        assertEq(resolver.balance, 1 ether);
        assertEq(intents.deposits(id), 0);
    }

    function test_keeperRefundingAbandonedFillEarnsDeposit() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        // resolver vanishes; nobody reveals the secret
        vm.warp(block.timestamp + 6 hours);

        vm.prank(keeper);
        intents.refund(id);

        assertEq(usdc.balanceOf(maker), SRC, "maker made whole without paying gas");
        assertEq(keeper.balance, DEPOSIT, "keeper paid for the refund");
        assertEq(resolver.balance, 1 ether - DEPOSIT, "resolver forfeits its deposit");
    }

    function test_depositPaidOnce() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        vm.warp(block.timestamp + 6 hours);
        vm.prank(keeper);
        intents.refund(id);
        vm.expectRevert();
        intents.refund(id);
        assertEq(keeper.balance, DEPOSIT);
    }

    function test_extraValueIsAllDeposit() public {
        bytes32 id = _fill(resolver, 3 * DEPOSIT);
        assertEq(intents.deposits(id), 3 * DEPOSIT);
    }

    // ----------------------------------------------------------- SecretRelay

    function _relayed(address redeemer) internal returns (SecretRelay relay, bytes32 id) {
        MockCCIPRouter router = new MockCCIPRouter(1);
        relay = new SecretRelay(IRouterClient(address(router)), intents, address(this));
        address peer = makeAddr("remoteRelay");
        relay.setPeer(2, peer);
        vm.deal(redeemer, 1 ether);
        id = _fill(redeemer, DEPOSIT);

        bytes32[] memory ids = new bytes32[](1);
        bytes[] memory secrets = new bytes[](1);
        ids[0] = id;
        secrets[0] = secret;
        Client.Any2EVMMessage memory m = Client.Any2EVMMessage({
            messageId: bytes32(uint256(1)),
            sourceChainSelector: 2,
            sender: abi.encode(peer),
            data: abi.encode(ids, secrets),
            destTokenAmounts: new Client.EVMTokenAmount[](0)
        });
        vm.prank(address(router));
        relay.ccipReceive(m);
    }

    function test_relayForwardsDepositToRedeemer() public {
        (SecretRelay relay,) = _relayed(resolver);
        assertEq(usdc.balanceOf(resolver), SRC);
        assertEq(resolver.balance, 1 ether, "deposit returned through the relay");
        assertEq(address(relay).balance, 0);
    }

    function test_relayFallsBackToClaimWhenPushFails() public {
        EthRejecter rejecter = new EthRejecter();
        (SecretRelay relay,) = _relayed(address(rejecter));

        assertEq(usdc.balanceOf(address(rejecter)), SRC, "redemption still succeeded");
        assertEq(relay.unclaimed(address(rejecter)), DEPOSIT);
        // pulling still fails for a contract that rejects ETH, but the funds stay claimable
        vm.expectRevert();
        rejecter.claimFrom(relay);
        assertEq(relay.unclaimed(address(rejecter)), DEPOSIT);
    }

    function test_relayRejectsStrayEth() public {
        (SecretRelay relay,) = _relayed(resolver);
        vm.expectRevert(SecretRelay.OnlyHashLock.selector);
        payable(address(relay)).transfer(1);
    }
}
