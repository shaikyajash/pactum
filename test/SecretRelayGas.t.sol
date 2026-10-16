// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {PactumIntents} from "../src/PactumIntents.sol";
import {SecretRelay} from "../src/ccip/SecretRelay.sol";
import {Client, IRouterClient} from "../src/ccip/Client.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockCCIPRouter} from "./mocks/MockCCIPRouter.sol";

/// @dev Native redeemer whose receive() burns gas: mode 1 burns everything it
///      gets, mode 2 burns `work` gas and then accepts (an honest but expensive
///      wallet).
contract GasActor {
    uint8 public immutable mode;
    uint256 public immutable work;

    constructor(uint8 _mode, uint256 _work) {
        mode = _mode;
        work = _work;
    }

    receive() external payable {
        if (mode == 1) {
            while (true) {}
        }
        uint256 g = gasleft();
        while (g - gasleft() < work) {}
    }
}

/// @notice Per-entry gas budgets in SecretRelay (audit-2 M-1) and measurements
///         backing BASE_GAS, PER_ENTRY_OVERHEAD and MIN_ENTRY_GAS.
contract SecretRelayGasTest is Test {
    uint64 internal constant SEL_A = 1;
    uint64 internal constant SEL_B = 2;
    uint256 internal constant DEPOSIT = 0.01 ether;
    uint256 internal constant SRC = 100e18;
    uint256 internal constant BATCH = 8;

    PactumIntents internal intents;
    MockERC20 internal usdc;
    MockCCIPRouter internal routerA;
    MockCCIPRouter internal routerB;
    SecretRelay internal relay;
    SecretRelay internal relayB;
    address internal peer;

    uint256 internal makerKey = 0xA11CE;
    address internal maker = vm.addr(makerKey);
    uint256 internal nonce;

    bytes32[] internal fillIds;
    bytes[] internal fillSecrets;

    function setUp() public {
        vm.warp(1_800_000_000);
        intents = new PactumIntents();
        usdc = new MockERC20("USD Coin", "USDC");
        usdc.mint(maker, BATCH * SRC);
        vm.prank(maker);
        usdc.approve(address(intents), type(uint256).max);

        routerA = new MockCCIPRouter(SEL_A);
        routerB = new MockCCIPRouter(SEL_B);
        routerA.connect(routerB);
        routerB.connect(routerA);
        relay = new SecretRelay(IRouterClient(address(routerA)), intents, address(this));
        relayB = new SecretRelay(IRouterClient(address(routerB)), intents, address(this));
        peer = address(relayB);
        relay.setPeer(SEL_B, peer);
        relayB.setPeer(SEL_A, address(relay));

        // worst-case ERC20 entries: each pays a fresh resolver (zero -> nonzero token
        // balance and deposit credit)
        for (uint256 i; i < BATCH; ++i) {
            bytes memory s = abi.encode(keccak256(abi.encode("secret", i)));
            fillIds.push(_fill(address(uint160(0x5000 + i)), s));
            fillSecrets.push(s);
        }
    }

    // ------------------------------------------------------------- helpers

    function _fill(address filler, bytes memory s) internal returns (bytes32 id) {
        bytes32 h = sha256(s);
        PactumIntents.Order memory o = PactumIntents.Order({
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
            parts: 1,
            secretsRoot: keccak256(bytes.concat(keccak256(abi.encode(uint256(0), h)))),
            nonce: ++nonce,
            minDeposit: DEPOSIT
        });
        (uint8 v, bytes32 r, bytes32 ss) = vm.sign(makerKey, intents.hashOrder(o));
        vm.deal(filler, DEPOSIT);
        vm.prank(filler);
        id = intents.fill{value: DEPOSIT}(
            o, abi.encodePacked(r, ss, v), SRC, 0, h, new bytes32[](0)
        );
    }

    function _native(address redeemer, bytes memory s) internal returns (bytes32) {
        vm.deal(address(this), 1);
        return intents.initiate{value: 1}(
            redeemer, address(0), 1, sha256(s), uint64(block.timestamp + 1 days), bytes32(++nonce)
        );
    }

    function _msg(uint256 entryGas, bytes32[] memory ids, bytes[] memory ss)
        internal
        view
        returns (Client.Any2EVMMessage memory)
    {
        return Client.Any2EVMMessage({
            messageId: bytes32(uint256(1)),
            sourceChainSelector: SEL_B,
            sender: abi.encode(peer),
            data: abi.encode(entryGas, ids, ss),
            destTokenAmounts: new Client.EVMTokenAmount[](0)
        });
    }

    function _call(uint256 entryGas, bytes32[] memory ids, bytes[] memory ss)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodeCall(SecretRelay.ccipReceive, (_msg(entryGas, ids, ss)));
    }

    /// Delivers like the CCIP router: exactly `gasLimit` for the receiver, cold state.
    function _deliver(uint256 gasLimit, bytes memory data) internal returns (bool ok) {
        _coolAll();
        vm.prank(address(routerA));
        (ok,) = address(relay).call{gas: gasLimit}(data);
    }

    function _coolAll() internal {
        vm.cool(address(relay));
        vm.cool(address(intents));
        vm.cool(address(usdc));
    }

    function _settled(bytes32 id) internal view returns (bool s) {
        (,, s,,,,) = intents.swaps(id);
    }

    function _firstN(uint256 n) internal view returns (bytes32[] memory ids, bytes[] memory ss) {
        ids = new bytes32[](n);
        ss = new bytes[](n);
        for (uint256 i; i < n; ++i) {
            (ids[i], ss[i]) = (fillIds[i], fillSecrets[i]);
        }
    }

    // -------------------------------------------------------- measurements

    /// MIN_ENTRY_GAS covers a cold ERC20 redeem that also credits a deposit.
    function test_minEntryGasCoversColdErc20Redeem() public {
        (bytes32 id, bytes memory secret) = (fillIds[0], fillSecrets[0]);
        _coolAll();
        uint256 g = gasleft();
        intents.redeem(id, secret);
        uint256 used = g - gasleft();
        emit log_named_uint("cold ERC20 redeem + deposit credit", used);
        assertLt(used * 7 / 5, relay.MIN_ENTRY_GAS(), "MIN_ENTRY_GAS keeps a 40% margin");

        // and it settles when given exactly MIN_ENTRY_GAS
        _coolAll();
        (bool ok,) = address(intents).call{gas: relay.MIN_ENTRY_GAS()}(
            abi.encodeCall(intents.redeem, (fillIds[1], fillSecrets[1]))
        );
        assertTrue(ok);
        assertTrue(_settled(fillIds[1]));
    }

    /// A delivery with exactly `gasLimitFor(n, MIN_ENTRY_GAS)` settles every entry of
    /// a cold batch, for every batch size up to BATCH.
    function test_gasLimitForCoversColdBatches() public {
        for (uint256 n = 1; n <= BATCH; ++n) {
            (bytes32[] memory ids, bytes[] memory ss) = _firstN(n);
            uint256 entryGas = relay.MIN_ENTRY_GAS();
            uint256 snap = vm.snapshotState();
            assertTrue(_deliver(relay.gasLimitFor(n, entryGas), _call(entryGas, ids, ss)));
            for (uint256 i; i < n; ++i) {
                assertTrue(_settled(ids[i]));
            }
            vm.revertToState(snap);
        }
    }

    /// Measures the real fixed and per-entry overhead of ccipReceive with entries that
    /// fail immediately (unknown swap), so nearly all gas is relay overhead.
    function test_measureOverheads() public {
        uint256 entryGas = relay.MIN_ENTRY_GAS();
        uint256[] memory used = new uint256[](3);
        for (uint256 k; k < 3; ++k) {
            uint256 n = k * 4 + 1; // 1, 5, 9 entries
            bytes32[] memory ids = new bytes32[](n);
            bytes[] memory ss = new bytes[](n);
            for (uint256 i; i < n; ++i) {
                ids[i] = keccak256(abi.encode("missing", i));
                ss[i] = abi.encode(keccak256(abi.encode("secret", i)));
            }
            bytes memory data = _call(entryGas, ids, ss);
            _coolAll();
            vm.prank(address(routerA));
            uint256 g = gasleft();
            (bool ok,) = address(relay).call(data);
            used[k] = g - gasleft();
            assertTrue(ok);
        }
        uint256 perEntry = (used[2] - used[0]) / 8;
        uint256 base = used[0] - perEntry;
        emit log_named_uint("measured fixed ccipReceive gas", base);
        emit log_named_uint("measured per-entry overhead", perEntry);
        assertLt(base * 3 / 2, relay.BASE_GAS(), "BASE_GAS keeps a 50% margin");
        assertLt(perEntry * 3 / 2, relay.PER_ENTRY_OVERHEAD(), "overhead keeps a 50% margin");
    }

    // ------------------------------------------------- audit-2 M-1 regressions

    /// A redeemer that burns all gas only fails its own entry; the honest entry
    /// behind it settles with the normal, computed gas limit.
    function test_gasBurnerCannotStarveFollowingEntry() public {
        GasActor burner = new GasActor(1, 0);
        bytes memory bs = abi.encode(keccak256("burner-secret"));
        bytes32 bad = _native(address(burner), bs);

        bytes32[] memory ids = new bytes32[](2);
        bytes[] memory ss = new bytes[](2);
        (ids[0], ids[1]) = (bad, fillIds[0]);
        (ss[0], ss[1]) = (bs, fillSecrets[0]);
        uint256 entryGas = relay.MIN_ENTRY_GAS();

        vm.recordLogs();
        assertTrue(_deliver(relay.gasLimitFor(2, entryGas), _call(entryGas, ids, ss)));
        Vm.Log[] memory logs = vm.getRecordedLogs();

        assertFalse(_settled(bad), "burner entry failed alone");
        assertTrue(_settled(fillIds[0]), "honest entry settled");
        assertEq(usdc.balanceOf(address(0x5000)), SRC);
        assertEq(intents.depositCredit(address(0x5000)), DEPOSIT);
        uint256 failed;
        uint256 relayed;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != address(relay)) continue;
            if (logs[i].topics[0] == SecretRelay.RelayFailed.selector) {
                assertEq(logs[i].topics[1], bad);
                failed++;
            }
            if (logs[i].topics[0] == SecretRelay.SecretRelayed.selector) {
                assertEq(logs[i].topics[1], fillIds[0]);
                relayed++;
            }
        }
        assertEq(failed, 1);
        assertEq(relayed, 1);
    }

    /// An under-gassed delivery reverts the whole message (so CCIP can manually
    /// re-execute it) instead of reporting success with entries silently skipped.
    function test_underGassedDeliveryNeverSwallowsEntries() public {
        // honest wallet whose receive() legitimately needs ~250k gas
        GasActor heavy = new GasActor(2, 250_000);
        bytes memory hs = abi.encode(keccak256("heavy-secret"));
        bytes32 id = _native(address(heavy), hs);
        bytes32[] memory ids = new bytes32[](1);
        bytes[] memory ss = new bytes[](1);
        (ids[0], ss[0]) = (id, hs);
        uint256 entryGas = 300_000;
        bytes memory data = _call(entryGas, ids, ss);
        uint256 limit = relay.gasLimitFor(1, entryGas);

        for (uint256 g = 20_000; g < limit; g += 5_000) {
            uint256 snap = vm.snapshotState();
            bool ok = _deliver(g, data);
            if (ok) assertTrue(_settled(id), "success implies settled");
            vm.revertToState(snap);
        }

        // short of the entry budget: an explicit InsufficientGas revert
        _coolAll();
        vm.prank(address(routerA));
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.InsufficientGas.selector, 0));
        relay.ccipReceive{gas: limit - entryGas / 2}(_msg(entryGas, ids, ss));

        assertTrue(_deliver(limit, data));
        assertTrue(_settled(id));
    }

    /// A later entry that would be short of its budget reverts the message too.
    function test_insufficientGasForLaterEntryReverts() public {
        // three entries that each really need most of their budget
        bytes32[] memory ids = new bytes32[](3);
        bytes[] memory ss = new bytes[](3);
        for (uint256 i; i < 3; ++i) {
            ss[i] = abi.encode(keccak256(abi.encode("heavy", i)));
            ids[i] = _native(address(new GasActor(2, 250_000)), ss[i]);
        }
        uint256 entryGas = 300_000;
        uint256 limit = relay.gasLimitFor(3, entryGas);

        // the first two entries fit, the third would run short: nothing settles
        _coolAll();
        vm.prank(address(routerA));
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.InsufficientGas.selector, 2));
        relay.ccipReceive{gas: limit - 200_000}(_msg(entryGas, ids, ss));
        for (uint256 i; i < 3; ++i) {
            assertFalse(_settled(ids[i]));
        }

        assertTrue(_deliver(limit, _call(entryGas, ids, ss)));
        for (uint256 i; i < 3; ++i) {
            assertTrue(_settled(ids[i]));
        }
    }

    function test_relayRejectsEntryGasBelowMinimum() public {
        (bytes32[] memory ids, bytes[] memory ss) = _firstN(1);
        uint256 min = relay.MIN_ENTRY_GAS();
        uint256 fee = routerB.FEE();
        vm.deal(address(this), 1 ether);
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.EntryGasTooLow.selector, min - 1, min));
        relayB.relay{value: fee}(SEL_A, ids, ss, min - 1);
        vm.expectRevert(abi.encodeWithSelector(SecretRelay.EntryGasTooLow.selector, min - 1, min));
        relayB.quote(SEL_A, ids, ss, min - 1);
    }

    /// relay() derives the CCIP gas limit from the per-entry budget and carries the
    /// budget in the message.
    function test_relayEncodesBudgetAndGasLimit() public {
        (bytes32[] memory ids, bytes[] memory ss) = _firstN(2);
        uint256 entryGas = 150_000;
        Client.EVM2AnyMessage memory expected = Client.EVM2AnyMessage({
            receiver: abi.encode(address(relay)),
            data: abi.encode(entryGas, ids, ss),
            tokenAmounts: new Client.EVMTokenAmount[](0),
            feeToken: address(0),
            extraArgs: Client._argsToBytes(
                Client.GenericExtraArgsV2({
                    gasLimit: relayB.gasLimitFor(2, entryGas), allowOutOfOrderExecution: true
                })
            )
        });
        assertEq(
            relayB.gasLimitFor(2, entryGas),
            relayB.BASE_GAS() + 2 * (entryGas * 64 / 63 + relayB.PER_ENTRY_OVERHEAD())
        );
        uint256 fee = routerB.FEE();
        vm.deal(address(this), 1 ether);
        vm.expectCall(
            address(routerB), fee, abi.encodeCall(IRouterClient.ccipSend, (SEL_A, expected))
        );
        relayB.relay{value: fee}(SEL_A, ids, ss, entryGas);

        // and the delivered message settles both entries
        routerA.deliverNext();
        assertTrue(_settled(ids[0]));
        assertTrue(_settled(ids[1]));
    }

    receive() external payable {}
}
