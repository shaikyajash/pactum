// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {HashLock} from "../HashLock.sol";
import {Client, IAny2EVMMessageReceiver, IERC165, IRouterClient} from "./Client.sol";

/// @title SecretRelay
/// @notice Chainlink CCIP application that carries revealed HTLC secrets to the
///         chain holding the other leg and redeems it there automatically.
///
/// @dev Why this is safe to bolt onto an HTLC:
///      CCIP is used for *liveness*, not *safety*. `HashLock.redeem` still checks
///      `sha256(secret) == secretHash` and always pays the swap's fixed redeemer,
///      so a forged, replayed or reordered message cannot move funds anywhere they
///      could not already go. If the message never arrives, the counter-party can
///      still redeem by hand, and the timelocks still protect both sides. The relay
///      only removes the need to watch the remote chain.
///
///      Flow (maker's escrow on chain A pays resolver R; R's lock on B pays the maker):
///        1. The secret is revealed on B when the maker's leg is redeemed.
///        2. Anyone calls `relay(selectorA, [swapIdA], [secret], entryGas)` on B's
///           SecretRelay, paying the CCIP fee in native gas.
///        3. A's SecretRelay receives it via the CCIP router and calls
///           `HashLock.redeem` on A, paying R. Failures (already redeemed,
///           refunded) are logged, not reverted, so one stale entry never blocks
///           the rest of a batch. Messages may execute out of order, since each
///           redemption is independent and idempotent.
///
///      Gas: every entry runs `HashLock.redeem` with exactly `entryGas`, a budget
///      carried in the message. An entry that burns its budget (e.g. a hostile
///      native redeemer) only fails itself. If the delivery has too little gas to
///      give an entry its full budget, the WHOLE message reverts with
///      `InsufficientGas`, so CCIP marks it failed and it can be manually
///      re-executed with more gas; it never "succeeds" with entries silently
///      skipped. Pick `entryGas` to cover the most expensive redeem in the batch
///      (a native payout to a contract redeemer can cost more than MIN_ENTRY_GAS).
///
///      Reentrancy: the relay holds no ETH and writes no state while receiving.
///      A token or redeemer reentering during `redeem` can only reach `relay`
///      (sends a new, self-paid message), the router-only `ccipReceive` or the
///      owner-only admin functions, so no guard is needed.
contract SecretRelay is IAny2EVMMessageReceiver, IERC165 {
    IRouterClient public immutable router;
    HashLock public immutable hashLock;

    /// @notice Fixed `ccipReceive` gas outside the per-entry loop (auth, decoding).
    ///         Measured ~35k (test/SecretRelayGas.t.sol).
    uint256 public constant BASE_GAS = 60_000;
    /// @notice Gas per entry on top of its `entryGas` budget (gas check, call setup,
    ///         event). Measured ~9k.
    uint256 public constant PER_ENTRY_OVERHEAD = 15_000;
    /// @notice Smallest accepted `entryGas`: a cold ERC20 redeem that credits a
    ///         deposit to a fresh redeemer measures ~100k with a plain ERC20; the
    ///         rest is headroom for proxied tokens such as USDC.
    uint256 public constant MIN_ENTRY_GAS = 150_000;

    address public owner;
    /// nominated by `transferOwnership`; becomes owner on `acceptOwnership`
    address public pendingOwner;
    /// trusted SecretRelay deployment on each remote chain, by CCIP chain selector
    mapping(uint64 chainSelector => address peer) public peers;

    event PeerSet(uint64 indexed chainSelector, address peer);
    event OwnershipTransferStarted(address indexed previous, address indexed next);
    event OwnershipTransferred(address indexed previous, address indexed next);
    event SecretsSent(
        bytes32 indexed messageId, uint64 indexed dstChainSelector, bytes32[] swapIds
    );
    event SecretRelayed(bytes32 indexed swapId, uint64 indexed srcChainSelector);
    event RelayFailed(bytes32 indexed swapId, uint64 indexed srcChainSelector, bytes reason);

    error NotOwner();
    error NotPendingOwner();
    error ZeroAddress();
    error OnlyRouter();
    error NoPeer(uint64 chainSelector);
    error UntrustedPeer(uint64 chainSelector, address sender);
    error LengthMismatch();
    error EmptyBatch();
    error InsufficientFee(uint256 required, uint256 provided);
    error RefundFailed();
    error EntryGasTooLow(uint256 entryGas, uint256 minimum);
    error InsufficientGas(uint256 index);

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(IRouterClient _router, HashLock _hashLock, address _owner) {
        if (_owner == address(0)) revert ZeroAddress();
        router = _router;
        hashLock = _hashLock;
        owner = _owner;
        emit OwnershipTransferred(address(0), _owner);
    }

    // ------------------------------------------------------------------ admin

    function setPeer(uint64 chainSelector, address peer) external onlyOwner {
        peers[chainSelector] = peer;
        emit PeerSet(chainSelector, peer);
    }

    /// @notice Nominate `next` as owner; it takes effect once `next` calls
    ///         `acceptOwnership`. Nominating address(0) cancels a pending transfer.
    function transferOwnership(address next) external onlyOwner {
        pendingOwner = next;
        emit OwnershipTransferStarted(owner, next);
    }

    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert NotPendingOwner();
        emit OwnershipTransferred(owner, msg.sender);
        owner = msg.sender;
        pendingOwner = address(0);
    }

    // ------------------------------------------------------------------- send

    /// @notice Native fee to relay a batch to `dstChainSelector`.
    function quote(
        uint64 dstChainSelector,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        uint256 entryGas
    ) external view returns (uint256) {
        return router.getFee(
            dstChainSelector, _message(dstChainSelector, swapIds, secrets, entryGas)
        );
    }

    /// @notice CCIP destination gas limit for a batch of `entries` with `entryGas` each.
    /// @dev Includes the 1/64 that EIP-150 withholds from each call, so every entry
    ///      that passes the `ccipReceive` gas check is guaranteed its full budget.
    function gasLimitFor(uint256 entries, uint256 entryGas) public pure returns (uint256) {
        return BASE_GAS + entries * (entryGas * 64 / 63 + PER_ENTRY_OVERHEAD);
    }

    /// @notice Send secrets for swaps living on `dstChainSelector`. Permissionless:
    ///         the secrets are already public on this chain and are checked on arrival.
    /// @param entryGas gas each `HashLock.redeem` gets on the destination, at least
    ///        `MIN_ENTRY_GAS`. The CCIP gas limit is derived from it (`gasLimitFor`).
    ///        Excess `msg.value` is refunded.
    function relay(
        uint64 dstChainSelector,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        uint256 entryGas
    ) external payable returns (bytes32 messageId) {
        Client.EVM2AnyMessage memory message =
            _message(dstChainSelector, swapIds, secrets, entryGas);
        uint256 fee = router.getFee(dstChainSelector, message);
        if (msg.value < fee) revert InsufficientFee(fee, msg.value);

        messageId = router.ccipSend{value: fee}(dstChainSelector, message);
        emit SecretsSent(messageId, dstChainSelector, swapIds);

        if (msg.value > fee) {
            (bool ok,) = msg.sender.call{value: msg.value - fee}("");
            if (!ok) revert RefundFailed();
        }
    }

    function _message(
        uint64 dstChainSelector,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        uint256 entryGas
    ) internal view returns (Client.EVM2AnyMessage memory) {
        if (swapIds.length == 0) revert EmptyBatch();
        if (swapIds.length != secrets.length) revert LengthMismatch();
        if (entryGas < MIN_ENTRY_GAS) revert EntryGasTooLow(entryGas, MIN_ENTRY_GAS);
        address peer = peers[dstChainSelector];
        if (peer == address(0)) revert NoPeer(dstChainSelector);
        return Client.EVM2AnyMessage({
            receiver: abi.encode(peer),
            data: abi.encode(entryGas, swapIds, secrets),
            tokenAmounts: new Client.EVMTokenAmount[](0),
            feeToken: address(0),
            extraArgs: Client._argsToBytes(
                Client.GenericExtraArgsV2({
                    gasLimit: gasLimitFor(swapIds.length, entryGas), allowOutOfOrderExecution: true
                })
            )
        });
    }

    // ---------------------------------------------------------------- receive

    function ccipReceive(Client.Any2EVMMessage calldata message) external override {
        if (msg.sender != address(router)) revert OnlyRouter();
        address sender = abi.decode(message.sender, (address));
        uint64 src = message.sourceChainSelector;
        if (sender == address(0) || peers[src] != sender) revert UntrustedPeer(src, sender);

        (uint256 entryGas, bytes32[] memory swapIds, bytes[] memory secrets) =
            abi.decode(message.data, (uint256, bytes32[], bytes[]));
        uint256 required = entryGas * 64 / 63 + PER_ENTRY_OVERHEAD;

        for (uint256 i; i < swapIds.length; ++i) {
            // never run an entry short of its budget: revert the whole message so
            // CCIP records a failure that can be manually re-executed with more gas
            if (gasleft() < required) revert InsufficientGas(i);
            // HashLock credits any safety deposit straight to the swap's redeemer,
            // so the relay never holds ETH on anyone's behalf
            try hashLock.redeem{gas: entryGas}(swapIds[i], secrets[i]) {
                emit SecretRelayed(swapIds[i], src);
            } catch (bytes memory reason) {
                emit RelayFailed(swapIds[i], src, reason);
            }
        }
    }

    function supportsInterface(bytes4 interfaceId) external pure override returns (bool) {
        return interfaceId == type(IAny2EVMMessageReceiver).interfaceId
            || interfaceId == type(IERC165).interfaceId;
    }
}
