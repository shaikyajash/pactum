// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";
import {PactumIntents} from "../src/PactumIntents.sol";
import {SafeTransfer} from "../src/lib/SafeTransfer.sol";
import {SecretRelay} from "../src/ccip/SecretRelay.sol";
import {Client, IRouterClient} from "../src/ccip/Client.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockCCIPRouter} from "./mocks/MockCCIPRouter.sol";

/// @dev A redeemer contract that refuses plain ETH transfers.
contract EthRejecter {
    receive() external payable {
        revert("no thanks");
    }
}

/// @dev A settler contract with no `receive` at all, e.g. a relay or a multicall.
contract NoReceiveCaller {
    function redeemFor(HashLock h, bytes32 id, bytes calldata s) external {
        h.redeem(id, s);
    }

    function refundFor(HashLock h, bytes32 id) external {
        h.refund(id);
    }

    function withdrawTo(HashLock h, address payable to) external {
        h.withdrawDepositsTo(to);
    }
}

/// @dev Tries to withdraw again from inside its own payout.
contract Reenterer {
    HashLock internal immutable h;
    bool internal entered;

    constructor(HashLock _h) {
        h = _h;
    }

    function withdraw() external {
        h.withdrawDeposits();
    }

    receive() external payable {
        if (entered) return;
        entered = true;
        try h.withdrawDeposits() {
            revert("double withdraw");
        } catch {}
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

    function test_resolverRedeemCreditsDeposit() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        vm.expectEmit(address(intents));
        emit HashLock.DepositCredited(id, resolver, DEPOSIT);
        vm.prank(resolver);
        intents.redeem(id, secret);

        assertEq(usdc.balanceOf(resolver), SRC);
        assertEq(intents.deposits(id), 0);
        assertEq(intents.depositCredit(resolver), DEPOSIT);
        assertEq(resolver.balance, 1 ether - DEPOSIT, "nothing pushed inline");

        vm.prank(resolver);
        intents.withdrawDeposits();
        assertEq(resolver.balance, 1 ether);
        assertEq(intents.depositCredit(resolver), 0);
        assertEq(address(intents).balance, 0);
    }

    /// audit-1 M-1: once the secret is public anyone can submit the redeem first;
    /// the deposit must still go to the swap's redeemer, not the front-runner.
    function test_frontRunRedeemDoesNotStealDeposit() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        address mev = makeAddr("mev");
        vm.prank(mev);
        intents.redeem(id, secret);

        assertEq(usdc.balanceOf(resolver), SRC);
        assertEq(intents.depositCredit(mev), 0, "front-runner earns nothing");
        assertEq(mev.balance, 0);
        assertEq(intents.depositCredit(resolver), DEPOSIT, "resolver keeps its deposit");
    }

    function test_keeperRefundSplitsDeposit() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        // resolver vanishes; nobody reveals the secret
        vm.warp(block.timestamp + 6 hours);

        uint256 tip = DEPOSIT * intents.KEEPER_TIP_BPS() / 10_000;
        vm.expectEmit(address(intents));
        emit HashLock.DepositCredited(id, keeper, tip);
        vm.expectEmit(address(intents));
        emit HashLock.DepositCredited(id, maker, DEPOSIT - tip);
        vm.prank(keeper);
        intents.refund(id);

        assertEq(usdc.balanceOf(maker), SRC, "maker made whole without paying gas");
        assertEq(intents.depositCredit(keeper), DEPOSIT / 10, "keeper earns a 10% tip");
        assertEq(intents.depositCredit(maker), DEPOSIT * 9 / 10, "maker compensated");
        assertEq(intents.depositCredit(resolver), 0, "resolver forfeits its deposit");
    }

    /// audit-1 M-2: a griefer that fills, never counter-locks and refunds itself
    /// at expiry recovers only the keeper tip; the maker gets the rest.
    function test_selfRefundGrieferLosesMostOfDeposit() public {
        address griefer = makeAddr("griefer");
        vm.deal(griefer, 1 ether);
        PactumIntents.Order memory o = _order(maker);
        bytes32 id = _fill(griefer, DEPOSIT);
        vm.warp(block.timestamp + 6 hours);
        vm.startPrank(griefer);
        intents.refund(id);
        intents.withdrawDeposits();
        vm.stopPrank();

        assertEq(griefer.balance, 1 ether - DEPOSIT * 9 / 10, "griefing costs 90%");
        assertEq(intents.depositCredit(maker), DEPOSIT * 9 / 10);
        // the fill stays recorded: its secret may have leaked, so it is never reopened
        (uint256 filled,,) = intents.fills(intents.hashOrder(o));
        assertEq(filled, SRC);
    }

    function test_makerRefundingItselfGetsWholeDeposit() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        vm.warp(block.timestamp + 6 hours);
        vm.prank(maker);
        intents.refund(id);
        assertEq(intents.depositCredit(maker), DEPOSIT);
    }

    function test_instantRefundCreditsRedeemer() public {
        uint256 resolverKey = 0x5E5;
        address signer = vm.addr(resolverKey);
        vm.deal(signer, 1 ether);
        bytes32 id = _fill(signer, DEPOSIT);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(resolverKey, intents.refundDigest(id));
        vm.prank(keeper);
        intents.instantRefund(id, abi.encodePacked(r, s, v));

        assertEq(usdc.balanceOf(maker), SRC);
        assertEq(intents.depositCredit(signer), DEPOSIT, "consenting redeemer gets it back");
        assertEq(intents.depositCredit(keeper), 0);
    }

    function test_depositCreditedOnce() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        vm.warp(block.timestamp + 6 hours);
        vm.prank(keeper);
        intents.refund(id);
        vm.expectRevert(HashLock.AlreadySettled.selector);
        intents.refund(id);
        assertEq(intents.depositCredit(keeper) + intents.depositCredit(maker), DEPOSIT);
    }

    function test_extraValueIsAllDeposit() public {
        bytes32 id = _fill(resolver, 3 * DEPOSIT);
        assertEq(intents.deposits(id), 3 * DEPOSIT);
    }

    // --------------------------------------------- ETH-rejecting settlers

    /// audit-1 L-2: a contract without `receive` (e.g. a relay) can settle swaps
    /// that carry a deposit.
    function test_contractCallerWithoutReceiveCanRedeem() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        NoReceiveCaller c = new NoReceiveCaller();
        c.redeemFor(intents, id, secret);
        assertEq(usdc.balanceOf(resolver), SRC);
        assertEq(intents.depositCredit(resolver), DEPOSIT);
    }

    function test_contractCallerWithoutReceiveCanRefund() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        vm.warp(block.timestamp + 6 hours);
        NoReceiveCaller c = new NoReceiveCaller();
        c.refundFor(intents, id);
        assertEq(usdc.balanceOf(maker), SRC);
        assertEq(intents.depositCredit(address(c)), DEPOSIT / 10);
        // it can still route its tip to an address that accepts ETH
        c.withdrawTo(intents, payable(keeper));
        assertEq(keeper.balance, DEPOSIT / 10);
    }

    function test_settlementSucceedsWhenRedeemerRejectsEth() public {
        EthRejecter rejecter = new EthRejecter();
        vm.deal(address(rejecter), 1 ether);
        bytes32 id = _fill(address(rejecter), DEPOSIT);
        intents.redeem(id, secret);

        assertEq(usdc.balanceOf(address(rejecter)), SRC, "settlement not blocked");
        assertEq(intents.depositCredit(address(rejecter)), DEPOSIT);

        // pulling to itself fails, but the credit survives and can be routed elsewhere
        vm.prank(address(rejecter));
        vm.expectRevert(SafeTransfer.NativeTransferFailed.selector);
        intents.withdrawDeposits();
        assertEq(intents.depositCredit(address(rejecter)), DEPOSIT);

        vm.prank(address(rejecter));
        intents.withdrawDepositsTo(payable(keeper));
        assertEq(keeper.balance, DEPOSIT);
    }

    function test_refundSucceedsWhenMakerRejectsEth() public {
        EthRejecter rejecterMaker = new EthRejecter();
        bytes32 id = _fill(resolver, DEPOSIT);
        // make the swap initiator reject ETH as well
        vm.etch(maker, address(rejecterMaker).code);
        vm.warp(block.timestamp + 6 hours);
        vm.prank(keeper);
        intents.refund(id);
        assertEq(usdc.balanceOf(maker), SRC);
        assertEq(intents.depositCredit(maker), DEPOSIT * 9 / 10);
    }

    // ----------------------------------------------------------- withdrawals

    function test_withdrawCannotBeRepeated() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        intents.redeem(id, secret);
        vm.startPrank(resolver);
        intents.withdrawDeposits();
        vm.expectRevert(HashLock.NothingToWithdraw.selector);
        intents.withdrawDeposits();
        vm.expectRevert(HashLock.NothingToWithdraw.selector);
        intents.withdrawDepositsTo(payable(keeper));
        vm.stopPrank();
        assertEq(resolver.balance, 1 ether);
        assertEq(keeper.balance, 0);
    }

    function test_withdrawToSendsCallersCredit() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        intents.redeem(id, secret);
        vm.expectEmit(address(intents));
        emit HashLock.DepositsWithdrawn(resolver, keeper, DEPOSIT);
        vm.prank(resolver);
        intents.withdrawDepositsTo(payable(keeper));
        assertEq(keeper.balance, DEPOSIT);
        assertEq(intents.depositCredit(resolver), 0);
    }

    function test_withdrawToZeroAddressReverts() public {
        bytes32 id = _fill(resolver, DEPOSIT);
        intents.redeem(id, secret);
        vm.prank(resolver);
        vm.expectRevert(HashLock.ZeroAddress.selector);
        intents.withdrawDepositsTo(payable(address(0)));
    }

    function test_withdrawReentrancyCannotDoubleSpend() public {
        Reenterer r = new Reenterer(intents);
        vm.deal(address(r), 1 ether);
        bytes32 id = _fill(address(r), DEPOSIT);
        intents.redeem(id, secret);
        r.withdraw();
        assertEq(address(r).balance, 1 ether, "paid exactly once");
        assertEq(address(intents).balance, 0);
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

    function test_relayedRedeemCreditsRedeemer() public {
        (SecretRelay relay,) = _relayed(resolver);
        assertEq(usdc.balanceOf(resolver), SRC);
        assertEq(intents.depositCredit(resolver), DEPOSIT, "deposit credited to the redeemer");
        assertEq(intents.depositCredit(address(relay)), 0);
        assertEq(address(relay).balance, 0);
    }

    function test_relayedRedeemForEthRejectingRedeemer() public {
        EthRejecter rejecter = new EthRejecter();
        (SecretRelay relay,) = _relayed(address(rejecter));
        assertEq(usdc.balanceOf(address(rejecter)), SRC, "redemption still succeeded");
        assertEq(intents.depositCredit(address(rejecter)), DEPOSIT);
        assertEq(address(relay).balance, 0);
    }

    function test_relayRejectsStrayEth() public {
        (SecretRelay relay,) = _relayed(resolver);
        vm.expectRevert(SecretRelay.OnlyHashLock.selector);
        payable(address(relay)).transfer(1);
    }
}
