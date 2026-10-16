// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {HashLock} from "../src/HashLock.sol";
import {PactumIntents} from "../src/PactumIntents.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockWallet} from "./mocks/MockWallet.sol";
import {MockFeeERC20} from "./mocks/MockFeeERC20.sol";
import {TreeBuilder} from "./MerkleSecrets.t.sol";

contract PactumIntentsTest is Test {
    PactumIntents internal intents;
    MockERC20 internal usdc;

    uint256 internal makerKey = 0xA11CE;
    address internal maker = vm.addr(makerKey);
    address internal resolverA = makeAddr("resolverA");
    address internal resolverB = makeAddr("resolverB");

    uint16 internal constant PARTS = 4;
    uint256 internal constant SRC = 100e18;

    bytes[] internal secrets;
    bytes32[] internal hashes;

    function setUp() public {
        vm.warp(1_800_000_000);
        intents = new PactumIntents();
        usdc = new MockERC20("USD Coin", "USDC");
        usdc.mint(maker, 1_000e18);
        vm.prank(maker);
        usdc.approve(address(intents), type(uint256).max);

        for (uint256 i; i <= PARTS; ++i) {
            secrets.push(abi.encode("maker secret", i));
            hashes.push(sha256(secrets[i]));
        }
    }

    // ----------------------------------------------------------- helpers

    function _root(bytes32[] memory h) internal pure returns (bytes32 root) {
        (root,) = TreeBuilder.rootAndProof(h, 0);
    }

    function _proof(uint256 i) internal view returns (bytes32[] memory proof) {
        (, proof) = TreeBuilder.rootAndProof(hashes, i);
    }

    function _order(uint16 parts) internal view returns (PactumIntents.Order memory o) {
        bytes32[] memory h = hashes;
        if (parts == 1) {
            h = new bytes32[](1);
            h[0] = hashes[0];
        }
        o = PactumIntents.Order({
            maker: maker,
            receiver: maker,
            srcToken: address(usdc),
            srcAmount: SRC,
            dstChainId: 8453,
            dstToken: address(0),
            startDstAmount: 0.05 ether,
            endDstAmount: 0.03 ether,
            auctionStart: uint64(block.timestamp),
            auctionEnd: uint64(block.timestamp + 10 minutes),
            deadline: uint64(block.timestamp + 1 hours),
            srcLock: 6 hours,
            parts: parts,
            secretsRoot: _root(h),
            nonce: 1,
            minDeposit: 0
        });
    }

    function _sign(PactumIntents.Order memory o, uint256 key) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, intents.hashOrder(o));
        return abi.encodePacked(r, s, v);
    }

    function _fill(
        address resolver,
        PactumIntents.Order memory o,
        bytes memory sig,
        uint256 amount,
        uint256 idx
    ) internal returns (bytes32) {
        bytes32[] memory proof = _proof(idx);
        vm.prank(resolver);
        return intents.fill(o, sig, amount, idx, hashes[idx], proof);
    }

    // ------------------------------------------------------- happy paths

    function test_fullFill_singlePart() public {
        PactumIntents.Order memory o = _order(1);
        bytes memory sig = _sign(o, makerKey);
        bytes32[] memory proof = new bytes32[](0);

        vm.prank(resolverA);
        bytes32 id = intents.fill(o, sig, SRC, 0, hashes[0], proof);

        assertEq(usdc.balanceOf(address(intents)), SRC);
        (address init, uint64 exp,, address red,, uint256 amt, bytes32 h) = intents.swaps(id);
        assertEq(init, maker);
        assertEq(red, resolverA);
        assertEq(amt, SRC);
        assertEq(h, hashes[0]);
        assertEq(exp, block.timestamp + 6 hours);
    }

    function test_partialFills_byCompetingResolvers() public {
        PactumIntents.Order memory o = _order(PARTS);
        bytes memory sig = _sign(o, makerKey);

        bytes32 a = _fill(resolverA, o, sig, 30e18, 1); // 30% -> bucket 1
        bytes32 b = _fill(resolverB, o, sig, 70e18, 4); // 100% -> final secret

        (uint256 filled, uint32 next,) = intents.fills(intents.hashOrder(o));
        assertEq(filled, SRC);
        assertEq(next, 5);
        assertTrue(a != b);

        // each resolver gets paid only with its own secret
        intents.redeem(a, secrets[1]);
        intents.redeem(b, secrets[4]);
        assertEq(usdc.balanceOf(resolverA), 30e18);
        assertEq(usdc.balanceOf(resolverB), 70e18);
    }

    function test_signatureOnlyNeededForFirstFill() public {
        PactumIntents.Order memory o = _order(PARTS);
        _fill(resolverA, o, _sign(o, makerKey), 25e18, 0);
        _fill(resolverB, o, "", 75e18, 4);
        assertEq(usdc.balanceOf(address(intents)), SRC);
    }

    function test_refundGoesBackToMaker() public {
        PactumIntents.Order memory o = _order(1);
        bytes32[] memory proof = new bytes32[](0);
        vm.prank(resolverA);
        bytes32 id = intents.fill(o, _sign(o, makerKey), SRC, 0, hashes[0], proof);

        vm.warp(block.timestamp + 6 hours);
        intents.refund(id);
        assertEq(usdc.balanceOf(maker), 1_000e18);
    }

    function test_smartWalletMaker() public {
        MockWallet wallet = new MockWallet(maker);
        usdc.mint(address(wallet), SRC);
        wallet.approve(address(usdc), address(intents));

        PactumIntents.Order memory o = _order(1);
        o.maker = address(wallet);
        o.receiver = address(wallet);
        bytes32[] memory proof = new bytes32[](0);

        vm.prank(resolverA);
        intents.fill(o, _sign(o, makerKey), SRC, 0, hashes[0], proof);
        assertEq(usdc.balanceOf(address(wallet)), 0);
    }

    // ------------------------------------------------------------ auction

    function test_auctionDecaysLinearly() public {
        PactumIntents.Order memory o = _order(1);
        assertEq(intents.auctionPrice(o, o.auctionStart - 1), 0.05 ether);
        assertEq(intents.auctionPrice(o, o.auctionStart + 5 minutes), 0.04 ether);
        assertEq(intents.auctionPrice(o, o.auctionEnd), 0.03 ether);
        assertEq(intents.auctionPrice(o, o.auctionEnd + 1 days), 0.03 ether);
    }

    function test_fillEmitsOwedAmountAtFillTime() public {
        PactumIntents.Order memory o = _order(PARTS);
        bytes memory sig = _sign(o, makerKey);
        vm.warp(o.auctionStart + 5 minutes);

        bytes32 orderHash = intents.hashOrder(o);
        vm.expectEmit(true, false, true, true, address(intents));
        // 50% of the order at the mid-auction price of 0.04 ETH
        emit PactumIntents.OrderFilled(orderHash, 0, resolverA, 50e18, 1, 0.02 ether);
        _fill(resolverA, o, sig, 50e18, 1);
    }

    function test_owedRoundsUpForMaker() public view {
        PactumIntents.Order memory o = _order(PARTS);
        o.startDstAmount = 10;
        o.endDstAmount = 10;
        // 10 * 1 / 100e18 would round to 0; maker is owed at least 1 wei
        assertEq(intents.owedDstAmount(o, 1, 0), 1);
    }

    // ------------------------------------------------------------ reverts

    function test_reusingSecretBucketReverts() public {
        PactumIntents.Order memory o = _order(PARTS);
        bytes memory sig = _sign(o, makerKey);
        _fill(resolverA, o, sig, 10e18, 0);

        // 20% total is still bucket 0, whose secret may already be public
        bytes32[] memory proof = _proof(0);
        vm.prank(resolverB);
        vm.expectRevert(abi.encodeWithSelector(PactumIntents.WrongSecretIndex.selector, 0, 0));
        intents.fill(o, sig, 10e18, 0, hashes[0], proof);
    }

    function test_wrongIndexReverts() public {
        PactumIntents.Order memory o = _order(PARTS);
        bytes memory sig = _sign(o, makerKey);
        bytes32[] memory proof = _proof(3);
        vm.prank(resolverA);
        vm.expectRevert(abi.encodeWithSelector(PactumIntents.WrongSecretIndex.selector, 1, 3));
        intents.fill(o, sig, 50e18, 3, hashes[3], proof);
    }

    function test_badProofReverts() public {
        PactumIntents.Order memory o = _order(PARTS);
        bytes memory sig = _sign(o, makerKey);
        bytes32[] memory proof = _proof(2); // proof for a different leaf
        vm.prank(resolverA);
        vm.expectRevert(PactumIntents.InvalidProof.selector);
        intents.fill(o, sig, 50e18, 1, hashes[1], proof);
    }

    function test_overfillReverts() public {
        PactumIntents.Order memory o = _order(PARTS);
        bytes memory sig = _sign(o, makerKey);
        _fill(resolverA, o, sig, 60e18, 2);
        bytes32[] memory proof = _proof(4);
        vm.prank(resolverB);
        vm.expectRevert(PactumIntents.Overfill.selector);
        intents.fill(o, sig, 50e18, 4, hashes[4], proof);
    }

    function test_singlePartRejectsPartialFill() public {
        PactumIntents.Order memory o = _order(1);
        bytes memory sig = _sign(o, makerKey);
        bytes32[] memory proof = new bytes32[](0);
        vm.prank(resolverA);
        vm.expectRevert(PactumIntents.PartialFillNotAllowed.selector);
        intents.fill(o, sig, SRC / 2, 0, hashes[0], proof);
    }

    function test_forgedSignatureReverts() public {
        PactumIntents.Order memory o = _order(1);
        bytes memory sig = _sign(o, 0xBAD);
        bytes32[] memory proof = new bytes32[](0);
        vm.prank(resolverA);
        vm.expectRevert(HashLock.InvalidSignature.selector);
        intents.fill(o, sig, SRC, 0, hashes[0], proof);
    }

    function test_tamperedOrderReverts() public {
        PactumIntents.Order memory o = _order(1);
        bytes memory sig = _sign(o, makerKey);
        o.endDstAmount = 0; // resolver tries to lower the floor price
        bytes32[] memory proof = new bytes32[](0);
        vm.prank(resolverA);
        vm.expectRevert(HashLock.InvalidSignature.selector);
        intents.fill(o, sig, SRC, 0, hashes[0], proof);
    }

    function test_expiredOrderReverts() public {
        PactumIntents.Order memory o = _order(1);
        bytes memory sig = _sign(o, makerKey);
        vm.warp(o.deadline + 1);
        bytes32[] memory proof = new bytes32[](0);
        vm.prank(resolverA);
        vm.expectRevert(PactumIntents.OrderExpired.selector);
        intents.fill(o, sig, SRC, 0, hashes[0], proof);
    }

    /// audit-1 L-1: fills of fee-on-transfer tokens are rejected instead of
    /// recording an escrow larger than what the contract received.
    function test_fillRejectsFeeOnTransferToken() public {
        MockFeeERC20 fot = new MockFeeERC20(100); // 1% fee
        fot.mint(maker, SRC);
        vm.prank(maker);
        fot.approve(address(intents), type(uint256).max);

        PactumIntents.Order memory o = _order(1);
        o.srcToken = address(fot);
        bytes memory sig = _sign(o, makerKey);
        bytes32[] memory proof = new bytes32[](0);
        vm.prank(resolverA);
        vm.expectRevert(HashLock.UnsupportedToken.selector);
        intents.fill(o, sig, SRC, 0, hashes[0], proof);
        assertEq(fot.balanceOf(maker), SRC);
    }

    function test_cancel() public {
        PactumIntents.Order memory o = _order(PARTS);
        bytes memory sig = _sign(o, makerKey);

        vm.expectRevert(PactumIntents.NotMaker.selector);
        intents.cancel(o);

        vm.prank(maker);
        intents.cancel(o);

        bytes32[] memory proof = _proof(4);
        vm.prank(resolverA);
        vm.expectRevert(PactumIntents.OrderCancelledErr.selector);
        intents.fill(o, sig, SRC, 4, hashes[4], proof);
    }

    function test_badOrdersRejected() public {
        PactumIntents.Order memory o = _order(1);
        o.srcToken = address(0);
        bytes memory sig = _sign(o, makerKey);
        bytes32[] memory proof = new bytes32[](0);
        vm.prank(resolverA);
        vm.expectRevert(PactumIntents.BadOrder.selector);
        intents.fill(o, sig, SRC, 0, hashes[0], proof);

        o = _order(1);
        o.endDstAmount = o.startDstAmount + 1; // price must not increase
        sig = _sign(o, makerKey);
        vm.prank(resolverA);
        vm.expectRevert(PactumIntents.BadOrder.selector);
        intents.fill(o, sig, SRC, 0, hashes[0], proof);
    }

    // --------------------------------------------------------------- fuzz

    function testFuzz_secretIndexInRangeAndMonotonic(
        uint256 src,
        uint16 parts,
        uint256 a,
        uint256 b
    ) public view {
        src = bound(src, 1, 1e30);
        parts = uint16(bound(parts, 2, 1024));
        a = bound(a, 1, src);
        b = bound(b, a, src);
        uint256 ia = intents.secretIndexFor(src, parts, a);
        uint256 ib = intents.secretIndexFor(src, parts, b);
        assertLe(ia, ib);
        assertLe(ib, parts);
        if (b == src) assertEq(ib, parts);
        else assertLt(ib, parts);
    }

    function testFuzz_auctionBounded(uint64 t) public view {
        PactumIntents.Order memory o = _order(1);
        uint256 p = intents.auctionPrice(o, t);
        assertLe(p, o.startDstAmount);
        assertGe(p, o.endDstAmount);
    }
}
