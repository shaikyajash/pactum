// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {PactumIntents} from "../src/PactumIntents.sol";
import {MockPermitERC20} from "./mocks/MockPermitERC20.sol";
import {TreeBuilder} from "./MerkleSecrets.t.sol";

/// @notice The maker holds tokens but has never sent a transaction: no approve.
contract PactumIntentsPermitTest is Test {
    PactumIntents internal intents;
    MockPermitERC20 internal usdc;

    uint256 internal makerKey = 0xA11CE;
    address internal maker = vm.addr(makerKey);
    address internal resolver = makeAddr("resolver");

    uint256 internal constant SRC = 100e18;
    bytes32[] internal hashes;

    function setUp() public {
        vm.warp(1_800_000_000);
        intents = new PactumIntents();
        usdc = new MockPermitERC20("USD Coin", "USDC");
        usdc.mint(maker, SRC);
        for (uint256 i; i <= 4; ++i) {
            hashes.push(sha256(abi.encode(i)));
        }
    }

    function _order(uint16 parts) internal view returns (PactumIntents.Order memory o) {
        bytes32[] memory h = hashes;
        if (parts == 1) {
            h = new bytes32[](1);
            h[0] = hashes[0];
        }
        (bytes32 root,) = TreeBuilder.rootAndProof(h, 0);
        o = PactumIntents.Order({
            maker: maker,
            receiver: maker,
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
            parts: parts,
            secretsRoot: root,
            nonce: 1,
            minDeposit: 0
        });
    }

    function _signOrder(PactumIntents.Order memory o) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(makerKey, intents.hashOrder(o));
        return abi.encodePacked(r, s, v);
    }

    function _permit(uint256 key, uint256 value)
        internal
        view
        returns (PactumIntents.Permit memory)
    {
        uint256 deadline = block.timestamp + 1 hours;
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                usdc.DOMAIN_SEPARATOR(),
                keccak256(
                    abi.encode(
                        usdc.PERMIT_TYPEHASH(),
                        maker,
                        address(intents),
                        value,
                        usdc.nonces(maker),
                        deadline
                    )
                )
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return PactumIntents.Permit(value, deadline, v, r, s);
    }

    function _fill(
        PactumIntents.Order memory o,
        uint256 amount,
        uint256 idx,
        PactumIntents.Permit memory p
    ) internal returns (bytes32) {
        bytes32[] memory h = hashes;
        if (o.parts == 1) {
            h = new bytes32[](1);
            h[0] = hashes[0];
        }
        (, bytes32[] memory proof) = TreeBuilder.rootAndProof(h, idx);
        bytes memory sig = _signOrder(o);
        vm.prank(resolver);
        return intents.fillWithPermit(o, sig, amount, idx, h[idx], proof, p);
    }

    function test_fillWithPermit_noPriorApproval() public {
        assertEq(usdc.allowance(maker, address(intents)), 0);
        bytes32 id = _fill(_order(1), SRC, 0, _permit(makerKey, SRC));

        assertEq(usdc.balanceOf(address(intents)), SRC);
        assertEq(usdc.nonces(maker), 1);
        (address init,,,,,,) = intents.swaps(id);
        assertEq(init, maker);
        // the maker has sent no transactions at all
        assertEq(vm.getNonce(maker), 0);
    }

    function test_fillWithPermit_survivesFrontRunPermit() public {
        PactumIntents.Permit memory p = _permit(makerKey, SRC);
        // a griefer submits the permit directly first
        usdc.permit(maker, address(intents), p.value, p.deadline, p.v, p.r, p.s);

        _fill(_order(1), SRC, 0, p);
        assertEq(usdc.balanceOf(address(intents)), SRC);
    }

    function test_fillWithPermit_reusedAcrossPartialFills() public {
        PactumIntents.Order memory o = _order(4);
        PactumIntents.Permit memory p = _permit(makerKey, SRC);

        _fill(o, 30e18, 1, p);
        // same permit again: already consumed, but the remaining allowance covers it
        _fill(o, 70e18, 4, p);
        assertEq(usdc.balanceOf(address(intents)), SRC);
    }

    function test_fillWithPermit_rejectsForgedPermit() public {
        PactumIntents.Order memory o = _order(1);
        PactumIntents.Permit memory p = _permit(0xBAD, SRC);
        (, bytes32[] memory proof) = TreeBuilder.rootAndProof(_single(), 0);
        bytes memory sig = _signOrder(o);
        vm.prank(resolver);
        vm.expectRevert(PactumIntents.PermitFailed.selector);
        intents.fillWithPermit(o, sig, SRC, 0, hashes[0], proof, p);
    }

    function _single() internal view returns (bytes32[] memory h) {
        h = new bytes32[](1);
        h[0] = hashes[0];
    }
}
